use archivist_apply::{ReviewTagOperations, resolve_review_patch, review_apply_baseline};
use archivist_core::DocumentPatch;
use archivist_paperless::PaperlessDocumentDetail;
use serde_json::json;

fn document() -> PaperlessDocumentDetail {
    PaperlessDocumentDetail {
        id: 42,
        title: Some("old title".to_owned()),
        created: Some("2026-01-02".to_owned()),
        modified: Some("2026-01-02T03:04:05Z".to_owned()),
        content: Some("old content".to_owned()),
        tags: vec![1, 2],
        correspondent: Some(7),
        document_type: Some(8),
        custom_fields: json!([
            {"field": 10, "value": "old"},
            {"field": 11, "value": 1}
        ]),
        original_file_name: Some("document.pdf".to_owned()),
    }
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
fn baseline_covers_every_patchable_field_without_raw_sensitive_values() {
    let original = document();
    let baseline = review_apply_baseline(&original);
    let object = baseline.as_object().expect("baseline object");
    for field in [
        "content",
        "title",
        "tags",
        "correspondent",
        "document_type",
        "created",
        "custom_fields",
    ] {
        assert!(object.contains_key(field), "missing baseline field {field}");
    }
    let serialized = baseline.to_string();
    assert!(!serialized.contains("old content"));
    assert!(!serialized.contains("old title"));
    assert!(!serialized.contains("\"old\""));
}

#[test]
fn every_scalar_patch_field_detects_a_newer_conflicting_edit() {
    let cases = [
        (
            "content",
            DocumentPatch {
                content: Some("review content".to_owned()),
                ..empty_patch()
            },
        ),
        (
            "title",
            DocumentPatch {
                title: Some("review title".to_owned()),
                ..empty_patch()
            },
        ),
        (
            "correspondent",
            DocumentPatch {
                correspondent: Some(Some(70)),
                ..empty_patch()
            },
        ),
        (
            "document_type",
            DocumentPatch {
                document_type: Some(Some(80)),
                ..empty_patch()
            },
        ),
        (
            "created",
            DocumentPatch {
                created: Some("2026-02-03".to_owned()),
                ..empty_patch()
            },
        ),
        (
            "custom_fields",
            DocumentPatch {
                custom_fields: Some(json!([{"field": 10, "value": "review"}])),
                ..empty_patch()
            },
        ),
    ];

    for (field, desired) in cases {
        let original = document();
        let baseline = review_apply_baseline(&original);
        let mut current = original;
        match field {
            "content" => current.content = Some("manual content".to_owned()),
            "title" => current.title = Some("manual title".to_owned()),
            "correspondent" => current.correspondent = Some(700),
            "document_type" => current.document_type = Some(800),
            "created" => current.created = Some("2026-03-04".to_owned()),
            "custom_fields" => current.custom_fields = json!([{"field": 10, "value": "manual"}]),
            _ => unreachable!(),
        }

        let conflict = resolve_review_patch(
            &baseline,
            desired,
            &current,
            &ReviewTagOperations::default(),
        )
        .expect_err("newer edit must conflict");
        assert_eq!(conflict.fields(), &[field.to_owned()]);
    }
}

#[test]
fn unrelated_changes_do_not_block_and_already_desired_values_are_pruned() {
    let original = document();
    let desired = DocumentPatch {
        title: Some("review title".to_owned()),
        ..empty_patch()
    };
    let baseline = review_apply_baseline(&original);

    let mut unrelated = original.clone();
    unrelated.correspondent = Some(999);
    unrelated.tags.push(99);
    let resolved = resolve_review_patch(
        &baseline,
        desired.clone(),
        &unrelated,
        &ReviewTagOperations::default(),
    )
    .expect("unrelated fields must not conflict");
    assert_eq!(resolved.title.as_deref(), Some("review title"));

    unrelated.title = Some("review title".to_owned());
    let resolved = resolve_review_patch(
        &baseline,
        desired,
        &unrelated,
        &ReviewTagOperations::default(),
    )
    .expect("already-applied desired value must not conflict");
    assert!(resolved.title.is_none());
}

#[test]
fn foreign_tags_are_preserved_by_three_way_merge() {
    let original = document();
    let desired = DocumentPatch {
        tags: Some(vec![1, 2, 3]),
        ..empty_patch()
    };
    let baseline = review_apply_baseline(&original);
    let mut current = original;
    current.tags = vec![1, 2, 99];

    let resolved = resolve_review_patch(
        &baseline,
        desired,
        &current,
        &ReviewTagOperations::default(),
    )
    .expect("foreign tag additions are mergeable");
    assert_eq!(resolved.tags, Some(vec![1, 2, 3, 99]));
}

#[test]
fn review_and_workflow_tag_deltas_apply_to_the_current_set() {
    let original = document();
    let desired = DocumentPatch {
        tags: Some(vec![1]),
        ..empty_patch()
    };
    let baseline = review_apply_baseline(&original);
    let mut current = original;
    current.tags = vec![1, 2, 99];

    let resolved = resolve_review_patch(
        &baseline,
        desired,
        &current,
        &ReviewTagOperations {
            additions: vec![10],
            removals: vec![1],
        },
    )
    .expect("tag deltas are mergeable");
    assert_eq!(resolved.tags, Some(vec![10, 99]));
}

#[test]
fn missing_baseline_for_an_edited_in_field_fails_closed() {
    let original = document();
    let baseline = json!({"tags": [1, 2]});
    let desired = DocumentPatch {
        title: Some("reviewer added this field".to_owned()),
        ..empty_patch()
    };

    let conflict = resolve_review_patch(
        &baseline,
        desired,
        &original,
        &ReviewTagOperations::default(),
    )
    .expect_err("a field without a creation baseline must fail closed");
    assert_eq!(conflict.fields(), &["title".to_owned()]);
}
