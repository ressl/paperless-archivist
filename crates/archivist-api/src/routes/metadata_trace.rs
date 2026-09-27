//! Metadata-trace diagnostic endpoint and its outcome decision tree.

use crate::*;

// ---------------------------------------------------------------------------
// Metadata-trace diagnostic endpoint (v1.5.21).
//
// `GET /api/inventory/{document_id}/metadata-trace`
//
// Returns the most recent metadata-stage `pipeline_runs` row for a document
// together with the LLM artifact, all review items, the apply-time audit
// event, and a six-entry `per_field_outcomes` array computed via the
// `compute_field_outcome` decision tree (see `docs/METADATA_TRACE_CONTRACT.md`).
//
// 404s when no metadata run exists yet; the frontend hides the diagnose drawer
// in that case.

/// One of the six metadata-stage fields the diagnostic UI surfaces. Order is
/// load-bearing — the frontend renders the six cards in this exact order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataField {
    Title,
    Correspondent,
    DocumentType,
    DocumentDate,
    Tags,
    Fields,
}

impl MetadataField {
    /// Canonical field name used in `MetadataFieldOutcome.field` AND in
    /// `review_items.suggested_patch.standard_metadata.field`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Correspondent => "correspondent",
            Self::DocumentType => "document_type",
            Self::DocumentDate => "document_date",
            Self::Tags => "tags",
            Self::Fields => "fields",
        }
    }

    /// All six fields in the contract-specified order.
    pub(crate) fn all() -> [Self; 6] {
        [
            Self::Title,
            Self::Correspondent,
            Self::DocumentType,
            Self::DocumentDate,
            Self::Tags,
            Self::Fields,
        ]
    }

    /// Key under which durable patch-intent audit events carry
    /// this field's value. `document_date` is keyed as `created` because the
    /// worker writes to Paperless's `created` field. `fields` is keyed as
    /// `custom_fields` (the Paperless API name).
    pub(crate) fn audit_key(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Correspondent => "correspondent",
            Self::DocumentType => "document_type",
            Self::DocumentDate => "created",
            Self::Tags => "tags",
            Self::Fields => "custom_fields",
        }
    }

    /// Returns whether this field currently has a value set on the document
    /// inventory snapshot. Used by the "skipped because overwrite disabled"
    /// branch.
    pub(crate) fn current_value_set(self, current: &CurrentState) -> bool {
        match self {
            Self::Title => current.title.as_deref().is_some_and(|v| !v.is_empty()),
            Self::Correspondent => current
                .correspondent
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            Self::DocumentType => current
                .document_type
                .as_deref()
                .is_some_and(|v| !v.is_empty()),
            Self::DocumentDate => current.document_date.is_some(),
            // Tags & custom-fields don't gate on overwrite. The metadata
            // stage ALWAYS merges into the existing tag set (see
            // OldTagStrategy::KeepExisting) and custom-fields stage just
            // adds entries, so "skipped because overwrite disabled" never
            // applies to either. Returning false here keeps the decision
            // tree from triggering branch 3 for these fields.
            Self::Tags | Self::Fields => false,
        }
    }

    /// Returns whether the metadata settings disallow overwriting an
    /// existing value for this field. Mirrors the four
    /// `overwrite_existing_*` flags. `title` and `tags`/`fields` are not
    /// gated by settings — overwrite is the only sensible behaviour — so we
    /// return `false` so branch 3 never triggers for them.
    pub(crate) fn overwrite_disabled(self, settings: &RuntimeSettings) -> bool {
        match self {
            Self::Correspondent => !settings.metadata.overwrite_existing_correspondent,
            Self::DocumentType => !settings.metadata.overwrite_existing_document_type,
            Self::DocumentDate => !settings.metadata.overwrite_existing_document_date,
            Self::Title | Self::Tags | Self::Fields => false,
        }
    }
}

/// Snapshot of `document_inventory` joined with `paperless_*` lookup tables,
/// reduced to the names the diagnostic UI needs. The IDs in
/// `document_inventory` are resolved to names so the frontend can render the
/// "current state" block without a second round-trip.
#[derive(Debug, Clone, Default)]
pub(crate) struct CurrentState {
    pub(crate) title: Option<String>,
    pub(crate) correspondent: Option<String>,
    pub(crate) document_type: Option<String>,
    /// Typed since migration 0043; serialized as "YYYY-MM-DD" in to_json.
    pub(crate) document_date: Option<chrono::NaiveDate>,
    pub(crate) tags: Vec<String>,
}

impl CurrentState {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "title": self.title,
            "correspondent": self.correspondent,
            "document_type": self.document_type,
            "document_date": self.document_date,
            "tags": self.tags,
        })
    }
}

/// Outcome of the metadata stage for a single field, ready to serialise as a
/// `MetadataFieldOutcome` object (see openapi/openapi.yaml).
#[derive(Debug, Clone)]
pub(crate) struct FieldOutcome {
    pub(crate) field: MetadataField,
    pub(crate) outcome: &'static str,
    pub(crate) value: Value,
    pub(crate) confidence: Option<f64>,
    pub(crate) reason: Option<&'static str>,
    pub(crate) warnings: Vec<Value>,
}

impl FieldOutcome {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "field": self.field.as_str(),
            "outcome": self.outcome,
            "value": self.value,
            "confidence": self.confidence,
            "reason": self.reason,
            "warnings": self.warnings,
        })
    }
}

/// Pure decision tree from `docs/METADATA_TRACE_CONTRACT.md` §"Outcome
/// composition rules (backend)". Kept side-effect-free so it is table-test
/// friendly — see the `metadata_trace_tests` module below.
///
/// Branch order matters: a review item ALWAYS wins over a bare audit row
/// because the worker creates review items before applying for the manual
/// review path. For full-auto, the worker writes the audit row directly and
/// skips review_items, so branch 2 takes over.
pub(crate) fn compute_field_outcome(
    field: MetadataField,
    review_items: &[&MetadataReviewItem],
    audit: Option<&MetadataApplyAudit>,
    current: &CurrentState,
    settings: &RuntimeSettings,
    llm_suggestion: Option<&Value>,
) -> FieldOutcome {
    // Filter review_items down to entries whose suggested_patch carries
    // `standard_metadata.field == <field>`. Worker code always sets this
    // attribute when creating per-field review items in the consolidated
    // metadata stage; legacy v1.3 per-field review items don't, but those
    // use a per-stage suggested_patch shape that the diagnostic frontend
    // already cannot render — so dropping them here is the correct fallback.
    let matching: Vec<&MetadataReviewItem> = review_items
        .iter()
        .copied()
        .filter(|item| review_item_targets_field(item, field))
        .collect();

    // Confidence + value extraction from the LLM suggestion. Used by
    // multiple branches so we compute them once up front.
    let suggestion_for_field = llm_suggestion
        .and_then(Value::as_object)
        .and_then(|object| object.get(field.as_str()));
    let suggestion_value = field_value_from_suggestion(field, suggestion_for_field);
    let suggestion_confidence = suggestion_for_field
        .and_then(Value::as_object)
        .and_then(|object| object.get("confidence"))
        .and_then(Value::as_f64);

    // ---- Branch 1: a review_item exists for this field. ------------------
    // Order of statuses matters: when multiple review_items exist for the
    // same field (the worker writes one per validation outcome plus an
    // `auto_validated` companion for the operator-review path), prefer the
    // most informative status: rejected > approved > applied > pending >
    // edited. This keeps the diagnostic surfacing the operator's final
    // decision even when the apply-time row lingers.
    if !matching.is_empty() {
        let chosen = pick_review_item(&matching);
        let warnings = warnings_from_value(&chosen.validation_warnings);
        return match chosen.status.as_str() {
            "pending" => FieldOutcome {
                field,
                outcome: "review",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: classify_review_reason(&warnings),
                warnings,
            },
            "approved" | "applied" | "edited" => FieldOutcome {
                field,
                outcome: "applied",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: None,
                warnings,
            },
            "rejected" => FieldOutcome {
                field,
                outcome: "rejected",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: Some("rejected_by_operator"),
                warnings,
            },
            // Unknown statuses fall through to the conservative "review"
            // outcome so the UI still surfaces something rather than 500.
            _ => FieldOutcome {
                field,
                outcome: "review",
                value: review_item_value(field, chosen).unwrap_or_else(|| suggestion_value.clone()),
                confidence: review_item_confidence(chosen).or(suggestion_confidence),
                reason: classify_review_reason(&warnings),
                warnings,
            },
        };
    }

    // ---- Branch 2: audit `after` payload carries this field. -------------
    if let Some(audit) = audit
        && let Some(after) = audit.after.as_ref().and_then(Value::as_object)
        && after.contains_key(field.audit_key())
    {
        return FieldOutcome {
            field,
            outcome: "applied",
            value: suggestion_value,
            confidence: suggestion_confidence,
            reason: None,
            warnings: Vec::new(),
        };
    }

    // ---- Branch 3: current value present + overwrite disabled. -----------
    if field.current_value_set(current) && field.overwrite_disabled(settings) {
        return FieldOutcome {
            field,
            outcome: "skipped",
            value: Value::Null,
            confidence: None,
            reason: Some("overwrite_disabled"),
            warnings: Vec::new(),
        };
    }

    // ---- Branch 4: LLM omitted the field entirely. -----------------------
    if suggestion_for_field.is_none() || suggestion_for_field == Some(&Value::Null) {
        return FieldOutcome {
            field,
            outcome: "dropped",
            value: Value::Null,
            confidence: None,
            reason: Some("no_proposal"),
            warnings: Vec::new(),
        };
    }

    // ---- Branch 5: LLM proposed but entity didn't resolve. ---------------
    //
    // Distinguishes correspondent / document_type "entity_not_found" from
    // generic "below_threshold" by looking at the confidence. If the
    // suggestion was confident enough that validation would have passed
    // but no review_item exists AND nothing was applied, the only way to
    // arrive here for those two fields is the worker's "named_entity_id_for_name
    // returned None" path — captured by `skipped_fields.push(<field>)` in the
    // worker — which is the entity-not-found case. For other fields (title,
    // document_date, tags, fields) the fallback reason is parse_failure.
    let reason: &'static str = match field {
        MetadataField::Correspondent | MetadataField::DocumentType => "entity_not_found",
        _ => "parse_failure",
    };
    FieldOutcome {
        field,
        outcome: "skipped",
        value: suggestion_value,
        confidence: suggestion_confidence,
        reason: Some(reason),
        warnings: Vec::new(),
    }
}

/// Returns whether the review_item's `suggested_patch.standard_metadata.field`
/// matches the requested metadata field.
pub(crate) fn review_item_targets_field(item: &MetadataReviewItem, field: MetadataField) -> bool {
    item.suggested_patch
        .as_object()
        .and_then(|object| object.get("standard_metadata"))
        .and_then(Value::as_object)
        .and_then(|object| object.get("field"))
        .and_then(Value::as_str)
        == Some(field.as_str())
}

/// Pick the most informative review_item from a set keyed to the same field.
/// See `compute_field_outcome` for the priority order.
pub(crate) fn pick_review_item<'a>(items: &[&'a MetadataReviewItem]) -> &'a MetadataReviewItem {
    let priority = |status: &str| -> u8 {
        match status {
            "rejected" => 5,
            "approved" => 4,
            "applied" => 3,
            "pending" => 2,
            "edited" => 1,
            _ => 0,
        }
    };
    items
        .iter()
        .copied()
        .max_by_key(|item| {
            (
                priority(&item.status),
                // Within the same status pick the most recent.
                item.created_at,
            )
        })
        .expect("matching is non-empty by caller invariant")
}

/// Extracts the LLM-suggested confidence for this review_item, if its
/// `suggested_patch.standard_metadata.confidence` carries one.
pub(crate) fn review_item_confidence(item: &MetadataReviewItem) -> Option<f64> {
    item.suggested_patch
        .as_object()
        .and_then(|object| object.get("standard_metadata"))
        .and_then(Value::as_object)
        .and_then(|object| object.get("confidence"))
        .and_then(Value::as_f64)
}

/// Best-effort value extraction from a review_item. Returns `None` when the
/// shape doesn't match what the worker writes; callers fall back to the LLM
/// suggestion in that case.
pub(crate) fn review_item_value(field: MetadataField, item: &MetadataReviewItem) -> Option<Value> {
    let object = item.suggested_patch.as_object()?;
    // Prefer the human-readable `suggested_name` / `suggested_date` /
    // `suggested_names` from `standard_metadata` so the UI shows names
    // rather than ID integers. Falls back to the raw patch fields for
    // legacy worker code that didn't set the friendly hints.
    let standard = object.get("standard_metadata").and_then(Value::as_object);
    match field {
        MetadataField::Title => object.get("title").cloned(),
        MetadataField::Correspondent => standard
            .and_then(|s| s.get("suggested_name"))
            .cloned()
            .or_else(|| object.get("correspondent").cloned()),
        MetadataField::DocumentType => standard
            .and_then(|s| s.get("suggested_name"))
            .cloned()
            .or_else(|| object.get("document_type").cloned()),
        MetadataField::DocumentDate => standard
            .and_then(|s| s.get("suggested_date"))
            .cloned()
            .or_else(|| object.get("created").cloned()),
        MetadataField::Tags => standard
            .and_then(|s| s.get("suggested_names"))
            .cloned()
            .or_else(|| object.get("tags").cloned()),
        MetadataField::Fields => standard
            .and_then(|s| s.get("suggested_names"))
            .cloned()
            .or_else(|| object.get("custom_fields").cloned()),
    }
}

/// Returns the canonical `reason` string for a review_item by inspecting its
/// validation_warnings array. The worker serialises each warning as a
/// `ValidationError` JSON tag — we pick the first recognised one and map to
/// the contract's canonical reason word.
pub(crate) fn classify_review_reason(warnings: &[Value]) -> Option<&'static str> {
    for warning in warnings {
        match warning {
            Value::String(text) if text.eq_ignore_ascii_case("LowConfidence") => {
                return Some("below_threshold");
            }
            Value::Object(map) => {
                if map.contains_key("LowConfidence") {
                    return Some("below_threshold");
                }
                if map.contains_key("UnknownChoice") {
                    return Some("entity_not_found");
                }
                if map.contains_key("UnknownTag") {
                    return Some("entity_not_found");
                }
                if map.contains_key("InvalidDate") {
                    return Some("parse_failure");
                }
                if map.contains_key("TooManyTags") {
                    return Some("over_max_tags");
                }
                if let Some(quality) = map.get("DataQuality").and_then(Value::as_str)
                    && quality.contains("anchor")
                {
                    return Some("anchor_missing");
                }
            }
            _ => {}
        }
    }
    None
}

/// `review_items.validation_warnings` is stored as JSON. Coerce it to a Vec
/// for the response while preserving the per-warning shape so the frontend
/// can render details.
pub(crate) fn warnings_from_value(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

/// Pull the field's canonical value out of `ai_artifacts.normalized_output`.
/// Returns `Value::Null` when the LLM omitted the field.
pub(crate) fn field_value_from_suggestion(
    field: MetadataField,
    suggestion: Option<&Value>,
) -> Value {
    let Some(object) = suggestion.and_then(Value::as_object) else {
        return Value::Null;
    };
    let value = match field {
        MetadataField::Title => object.get("title"),
        MetadataField::Correspondent | MetadataField::DocumentType => object.get("name"),
        MetadataField::DocumentDate => object.get("date"),
        MetadataField::Tags => object.get("tags"),
        MetadataField::Fields => object.get("fields"),
    };
    value.cloned().unwrap_or(Value::Null)
}

pub(crate) async fn load_current_state(
    pool: &DbPool,
    paperless_document_id: i32,
) -> Result<CurrentState> {
    // `document_inventory` only carries the correspondent / document_type
    // INTEGER IDs and the raw `current_tags` text array. The diagnostic
    // surfaces names — resolve via the cached `paperless_*` lookup tables.
    let row = sqlx::query(
        r#"
        select di.title,
               di.current_tags,
               di.document_date,
               c.name as correspondent_name,
               t.name as document_type_name
          from document_inventory di
          left join paperless_correspondents c on c.id = di.correspondent_id
          left join paperless_document_types t on t.id = di.document_type_id
         where di.paperless_document_id = $1
        "#,
    )
    .bind(paperless_document_id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = row {
        let current_tags: Vec<String> = row.try_get("current_tags")?;
        Ok(CurrentState {
            title: row.try_get("title")?,
            correspondent: row.try_get("correspondent_name")?,
            document_type: row.try_get("document_type_name")?,
            document_date: row.try_get("document_date")?,
            tags: current_tags,
        })
    } else {
        // Document not in the local inventory yet (e.g. the run was queued
        // before the first paperless sync settled). Return a blank snapshot
        // so the diagnostic still renders the run header + per-field
        // outcomes.
        Ok(CurrentState::default())
    }
}

#[tracing::instrument(skip(state, auth), fields(user_id = tracing::field::Empty))]
pub(crate) async fn inventory_metadata_trace(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(paperless_document_id): Path<i32>,
) -> ApiResult<Response> {
    if let Some(user_id) = auth.0.user_id {
        Span::current().record("user_id", tracing::field::display(user_id));
    }

    let Some(header) = latest_metadata_run_for_document(&state.pool, paperless_document_id).await?
    else {
        // No metadata run yet — frontend hides the Diagnose button until the
        // first run completes, but this is also the canonical 404 shape.
        let body = Json(json!({ "error": "no metadata run for this document" }));
        return Ok((StatusCode::NOT_FOUND, body).into_response());
    };

    let (artifact, review_items, audit, current, settings) = tokio::try_join!(
        latest_metadata_artifact_for_run(&state.pool, header.run_id),
        metadata_review_items_for_run(&state.pool, header.run_id),
        latest_apply_audit_for_run(&state.pool, header.run_id),
        load_current_state(&state.pool, paperless_document_id),
        get_runtime_settings(&state.pool),
    )?;

    let llm_suggestion: Option<Value> = artifact
        .as_ref()
        .and_then(|artifact| artifact.normalized_output.clone());
    let review_refs: Vec<&MetadataReviewItem> = review_items.iter().collect();
    let audit_ref = audit.as_ref();
    let outcomes: Vec<Value> = MetadataField::all()
        .iter()
        .map(|field| {
            compute_field_outcome(
                *field,
                &review_refs,
                audit_ref,
                &current,
                &settings,
                llm_suggestion.as_ref(),
            )
            .to_json()
        })
        .collect();

    let response = metadata_trace_response_body(
        paperless_document_id,
        &header,
        artifact.as_ref(),
        audit_ref,
        &current,
        outcomes,
        llm_suggestion.clone(),
    );
    Ok((StatusCode::OK, Json(response)).into_response())
}

pub(crate) fn metadata_trace_response_body(
    paperless_document_id: i32,
    header: &MetadataRunHeader,
    artifact: Option<&MetadataArtifact>,
    audit: Option<&MetadataApplyAudit>,
    current: &CurrentState,
    outcomes: Vec<Value>,
    llm_suggestion: Option<Value>,
) -> Value {
    let applied_at = audit
        .filter(|audit| audit.outcome == "success")
        .map(|audit| audit.created_at);

    json!({
        "paperless_document_id": paperless_document_id,
        "current_state": current.to_json(),
        "latest_run": {
            "run_id": header.run_id,
            "stage": "metadata",
            "status": header.status,
            "created_at": header.created_at,
            "applied_at": applied_at,
            "model": artifact.map(|a| a.model.clone()),
            "provider": artifact.map(|a| a.provider.clone()),
            "llm_suggestion": llm_suggestion,
            "per_field_outcomes": outcomes,
        }
    })
}

#[cfg(test)]
mod metadata_trace_tests {
    //! Table-driven unit tests for the metadata-trace outcome decision tree.
    //!
    //! Covers every branch of the 5-step tree from
    //! `docs/METADATA_TRACE_CONTRACT.md` so a regression in the worker's
    //! review_item shape or in the audit payload breaks tests at CI time
    //! rather than producing a confusing UI surface. Each case maps to one
    //! branch of `compute_field_outcome`.

    use super::*;
    use archivist_db::{MetadataApplyAudit, MetadataReviewItem};
    use chrono::TimeZone;
    use serde_json::json;
    use uuid::Uuid;

    /// Default runtime settings + a current_state with values set for the
    /// fields that have an `overwrite_existing_*` toggle. Used as the
    /// baseline for the table tests below.
    fn baseline_settings() -> RuntimeSettings {
        RuntimeSettings::default()
    }

    fn baseline_current_state() -> CurrentState {
        CurrentState {
            title: Some("Original Title".to_owned()),
            correspondent: Some("Existing Co".to_owned()),
            document_type: Some("Existing Type".to_owned()),
            document_date: chrono::NaiveDate::from_ymd_opt(2020, 1, 1),
            tags: vec!["existing".to_owned()],
        }
    }

    fn empty_current_state() -> CurrentState {
        CurrentState::default()
    }

    /// Helper for assembling a review_item that the decision tree should
    /// match against `field`.
    fn review_item(field: MetadataField, status: &str, suggested: Value) -> MetadataReviewItem {
        let mut patch = suggested;
        if let Some(object) = patch.as_object_mut() {
            object
                .entry("standard_metadata".to_owned())
                .or_insert_with(|| json!({ "field": field.as_str() }));
            if let Some(sm) = object
                .get_mut("standard_metadata")
                .and_then(Value::as_object_mut)
            {
                sm.insert("field".to_owned(), json!(field.as_str()));
            }
        }
        MetadataReviewItem {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            stage: "metadata".to_owned(),
            status: status.to_owned(),
            suggested_patch: patch,
            edited_patch: None,
            validation_warnings: json!([]),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 5, 17, 0, 0, 0).unwrap(),
        }
    }

    fn audit_with_after(after: Value) -> MetadataApplyAudit {
        MetadataApplyAudit {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            after: Some(after),
            outcome: "success".to_owned(),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 5, 17, 0, 0, 0).unwrap(),
        }
    }

    // -------- Branch 1a: review_item status == pending → review ---------
    #[test]
    fn pending_review_item_yields_review_outcome_with_below_threshold_reason() {
        let mut item = review_item(
            MetadataField::DocumentDate,
            "pending",
            json!({
                "created": "2026-04-15",
                "standard_metadata": {
                    "field": "document_date",
                    "suggested_date": "2026-04-15",
                    "confidence": 0.62,
                }
            }),
        );
        item.validation_warnings =
            json!([{ "LowConfidence": { "actual": 0.62, "threshold": 0.9 } }]);

        let outcome = compute_field_outcome(
            MetadataField::DocumentDate,
            &[&item],
            None,
            &empty_current_state(),
            &baseline_settings(),
            None,
        );
        assert_eq!(outcome.outcome, "review");
        assert_eq!(outcome.reason, Some("below_threshold"));
        assert_eq!(outcome.value, json!("2026-04-15"));
        assert_eq!(outcome.confidence, Some(0.62));
        assert_eq!(outcome.warnings.len(), 1);
    }

    // -------- Branch 1b: review_item status == approved → applied -------
    #[test]
    fn approved_review_item_yields_applied_outcome_with_value() {
        let item = review_item(
            MetadataField::Title,
            "approved",
            json!({
                "title": "Invoice 2026-04 Acme",
                "standard_metadata": { "field": "title", "auto_validated": true }
            }),
        );
        let outcome = compute_field_outcome(
            MetadataField::Title,
            &[&item],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({ "title": { "title": "Invoice 2026-04 Acme", "confidence": 0.92 } })),
        );
        assert_eq!(outcome.outcome, "applied");
        assert_eq!(outcome.value, json!("Invoice 2026-04 Acme"));
        assert_eq!(outcome.reason, None);
    }

    // -------- Branch 1c: review_item status == rejected → rejected ------
    #[test]
    fn rejected_review_item_yields_rejected_outcome_with_operator_reason() {
        let item = review_item(
            MetadataField::Correspondent,
            "rejected",
            json!({
                "correspondent": "",
                "standard_metadata": {
                    "field": "correspondent",
                    "suggested_name": "Acme Corp",
                    "confidence": 0.85,
                }
            }),
        );
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[&item],
            None,
            &empty_current_state(),
            &baseline_settings(),
            None,
        );
        assert_eq!(outcome.outcome, "rejected");
        assert_eq!(outcome.reason, Some("rejected_by_operator"));
        assert_eq!(outcome.value, json!("Acme Corp"));
    }

    // -------- Branch 2: audit `after` carries the field → applied -------
    #[test]
    fn audit_applied_field_yields_applied_outcome_when_no_review_item_exists() {
        let audit = audit_with_after(json!({
            "title": "Acme Invoice",
            "correspondent": 42,
            "tags": [1, 2, 3]
        }));
        let llm = json!({
            "title": { "title": "Acme Invoice", "confidence": 0.95 }
        });

        let outcome = compute_field_outcome(
            MetadataField::Title,
            &[],
            Some(&audit),
            &empty_current_state(),
            &baseline_settings(),
            Some(&llm),
        );
        assert_eq!(outcome.outcome, "applied");
        assert_eq!(outcome.value, json!("Acme Invoice"));
        assert_eq!(outcome.confidence, Some(0.95));
    }

    #[test]
    fn audit_applied_document_date_keyed_as_created() {
        let audit = audit_with_after(json!({ "created": "2026-04-15" }));
        let outcome = compute_field_outcome(
            MetadataField::DocumentDate,
            &[],
            Some(&audit),
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({
                "document_date": { "date": "2026-04-15", "confidence": 0.93 }
            })),
        );
        assert_eq!(outcome.outcome, "applied");
        assert_eq!(outcome.value, json!("2026-04-15"));
    }

    // -------- Branch 3: current value set + overwrite disabled → skipped ----
    #[test]
    fn skipped_when_overwrite_disabled_for_correspondent() {
        // No review item, no audit. Settings default has
        // overwrite_existing_correspondent = false.
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[],
            None,
            &baseline_current_state(),
            &baseline_settings(),
            Some(&json!({
                "correspondent": { "name": "New Co", "confidence": 0.99 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(outcome.reason, Some("overwrite_disabled"));
        assert_eq!(outcome.value, Value::Null);
    }

    #[test]
    fn overwrite_disabled_branch_does_not_fire_when_setting_is_on() {
        // Same as the previous test but with the override flipped on —
        // branch 3 must NOT fire. The decision tree falls through to
        // branch 5 (entity_not_found) because no audit / review row
        // exists, so the outcome is still "skipped" but with a different
        // reason. The test exists to prove branch 3 is gated correctly.
        let mut settings = baseline_settings();
        settings.metadata.overwrite_existing_correspondent = true;
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[],
            None,
            &baseline_current_state(),
            &settings,
            Some(&json!({
                "correspondent": { "name": "New Co", "confidence": 0.99 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(
            outcome.reason,
            Some("entity_not_found"),
            "branch 3 must not fire when overwrite_existing_correspondent is true"
        );
    }

    // -------- Branch 4: LLM omitted the field entirely → dropped --------
    #[test]
    fn dropped_when_llm_suggestion_omits_field() {
        let outcome = compute_field_outcome(
            MetadataField::Tags,
            &[],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({ "title": { "title": "x", "confidence": 0.9 } })),
        );
        assert_eq!(outcome.outcome, "dropped");
        assert_eq!(outcome.reason, Some("no_proposal"));
        assert_eq!(outcome.value, Value::Null);
        assert_eq!(outcome.confidence, None);
    }

    #[test]
    fn dropped_when_llm_artifact_is_absent_entirely() {
        for field in MetadataField::all() {
            let outcome = compute_field_outcome(
                field,
                &[],
                None,
                &empty_current_state(),
                &baseline_settings(),
                None,
            );
            assert_eq!(
                outcome.outcome,
                "dropped",
                "{} should be dropped",
                field.as_str()
            );
            assert_eq!(outcome.reason, Some("no_proposal"));
        }
    }

    // -------- Branch 5: LLM proposed but entity didn't resolve → skipped --
    #[test]
    fn skipped_entity_not_found_for_correspondent_when_no_audit_and_no_review() {
        // No audit, no review item, correspondent absent from current
        // state (so branch 3 doesn't fire) but LLM proposed a name.
        let outcome = compute_field_outcome(
            MetadataField::Correspondent,
            &[],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({
                "correspondent": { "name": "Ghost Co", "confidence": 0.95 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(outcome.reason, Some("entity_not_found"));
        assert_eq!(outcome.value, json!("Ghost Co"));
        assert_eq!(outcome.confidence, Some(0.95));
    }

    #[test]
    fn skipped_entity_not_found_for_document_type_when_no_audit_and_no_review() {
        let outcome = compute_field_outcome(
            MetadataField::DocumentType,
            &[],
            None,
            &empty_current_state(),
            &baseline_settings(),
            Some(&json!({
                "document_type": { "name": "Unknown", "confidence": 0.91 }
            })),
        );
        assert_eq!(outcome.outcome, "skipped");
        assert_eq!(outcome.reason, Some("entity_not_found"));
        assert_eq!(outcome.value, json!("Unknown"));
    }

    // -------- Branch ordering: review_item beats audit row --------------
    #[test]
    fn review_item_takes_precedence_over_audit_row() {
        // The metadata path can produce both: the auto_validated review
        // item AND the audit row. Branch 1 must win so the operator sees
        // the review status rather than a stale "applied" badge.
        let item = review_item(
            MetadataField::Title,
            "pending",
            json!({
                "title": "Pending Title",
                "standard_metadata": { "field": "title", "confidence": 0.8 }
            }),
        );
        let audit = audit_with_after(json!({ "title": "Audit Title" }));
        let outcome = compute_field_outcome(
            MetadataField::Title,
            &[&item],
            Some(&audit),
            &empty_current_state(),
            &baseline_settings(),
            None,
        );
        assert_eq!(outcome.outcome, "review");
        assert_eq!(outcome.value, json!("Pending Title"));
    }

    // -------- Field invariant: outcome ordering matches contract --------
    #[test]
    fn metadata_field_ordering_matches_contract() {
        let names: Vec<&'static str> = MetadataField::all()
            .iter()
            .map(|field| field.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "title",
                "correspondent",
                "document_type",
                "document_date",
                "tags",
                "fields",
            ]
        );
    }

    // -------- Response body assembly --------
    #[test]
    fn metadata_trace_response_body_carries_run_header_and_outcomes() {
        let header = MetadataRunHeader {
            run_id: Uuid::nil(),
            paperless_document_id: 4904,
            status: "succeeded".to_owned(),
            created_at: chrono::Utc
                .with_ymd_and_hms(2026, 5, 17, 13, 42, 11)
                .unwrap(),
            finished_at: None,
        };
        let audit = audit_with_after(json!({ "title": "Acme" }));
        let outcomes = MetadataField::all()
            .iter()
            .map(|field| json!({ "field": field.as_str(), "outcome": "dropped" }))
            .collect();
        let body = metadata_trace_response_body(
            4904,
            &header,
            None,
            Some(&audit),
            &CurrentState::default(),
            outcomes,
            None,
        );
        assert_eq!(body["paperless_document_id"], json!(4904));
        assert_eq!(body["latest_run"]["status"], json!("succeeded"));
        assert_eq!(body["latest_run"]["stage"], json!("metadata"));
        assert_eq!(
            body["latest_run"]["applied_at"].as_str(),
            Some("2026-05-17T00:00:00Z")
        );
        assert_eq!(
            body["latest_run"]["per_field_outcomes"]
                .as_array()
                .map(Vec::len),
            Some(6)
        );
    }

    #[test]
    fn metadata_trace_response_body_omits_applied_at_when_audit_failed() {
        let header = MetadataRunHeader {
            run_id: Uuid::nil(),
            paperless_document_id: 4904,
            status: "failed".to_owned(),
            created_at: chrono::Utc
                .with_ymd_and_hms(2026, 5, 17, 13, 42, 11)
                .unwrap(),
            finished_at: None,
        };
        let failed_audit = MetadataApplyAudit {
            id: Uuid::nil(),
            run_id: Uuid::nil(),
            after: Some(json!({ "title": "Acme" })),
            outcome: "failed".to_owned(),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 5, 17, 0, 0, 0).unwrap(),
        };
        let body = metadata_trace_response_body(
            4904,
            &header,
            None,
            Some(&failed_audit),
            &CurrentState::default(),
            Vec::new(),
            None,
        );
        assert!(body["latest_run"]["applied_at"].is_null());
    }
}
