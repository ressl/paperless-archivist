use archivist_core::DocumentPatch;
use serde_json::json;

#[test]
fn reference_patch_fields_distinguish_missing_null_and_ids() {
    for (value, expected) in [
        (json!({}), None),
        (
            json!({"correspondent": null, "document_type": null}),
            Some(None),
        ),
        (
            json!({"correspondent": 7, "document_type": 7}),
            Some(Some(7)),
        ),
    ] {
        let patch: DocumentPatch = serde_json::from_value(value.clone()).expect("decode patch");
        assert_eq!(patch.correspondent, expected);
        assert_eq!(patch.document_type, expected);
        assert_eq!(patch.is_empty(), expected.is_none());
        // Reviews and durable intents both persist the serialized patch.
        let stored = serde_json::to_value(&patch).expect("persist patch");
        assert_eq!(stored, value);
        let restored: DocumentPatch = serde_json::from_value(stored).expect("restore patch");
        assert_eq!(restored.correspondent, expected);
        assert_eq!(restored.document_type, expected);
    }
}

#[test]
fn reference_patch_fields_reject_invalid_types() {
    for field in ["correspondent", "document_type"] {
        for invalid in [json!("7"), json!(1.5), json!(true), json!([]), json!({})] {
            assert!(serde_json::from_value::<DocumentPatch>(json!({field: invalid})).is_err());
        }
    }
}
