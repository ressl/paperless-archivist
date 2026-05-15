use archivist_core::{detect_document_language, extract_issue_date_suggestion};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GoldenDocument {
    id: String,
    text: String,
    language: String,
    issue_date: String,
}

#[test]
fn golden_documents_match_language_and_issue_date_expectations() {
    let fixtures: Vec<GoldenDocument> =
        serde_json::from_str(include_str!("fixtures/golden_documents.json"))
            .expect("golden document fixtures");

    for document in fixtures {
        let detected = detect_document_language(&document.text);
        assert_eq!(
            detected.language, document.language,
            "language mismatch for {}",
            document.id
        );
        assert!(
            detected.confidence >= 0.35,
            "low language confidence for {}",
            document.id
        );

        let issue_date = extract_issue_date_suggestion(&document.text, &detected)
            .unwrap_or_else(|| panic!("missing issue date for {}", document.id));
        assert_eq!(
            issue_date.date, document.issue_date,
            "issue date mismatch for {}",
            document.id
        );
        assert!(
            issue_date.confidence.unwrap_or_default() >= 0.6,
            "low issue-date confidence for {}",
            document.id
        );
    }
}
