use archivist_core::{
    ChoiceSuggestion, detect_document_language, extract_issue_date_suggestion,
    validate_choice_suggestion, validate_document_date_suggestion,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct MetadataFixture {
    language: String,
    text: String,
    expected_correspondent: String,
    expected_document_type: String,
    expected_document_date: String,
}

#[test]
fn standard_metadata_fixture_eval_passes_for_dates_and_allowed_choices() {
    let fixtures: Vec<MetadataFixture> =
        serde_json::from_str(include_str!("fixtures/standard_metadata_samples.json")).unwrap();
    let allowed_correspondents = fixtures
        .iter()
        .map(|fixture| fixture.expected_correspondent.clone())
        .collect::<Vec<_>>();
    let allowed_document_types = fixtures
        .iter()
        .map(|fixture| fixture.expected_document_type.clone())
        .collect::<Vec<_>>();
    let mut evaluated = 0;

    for fixture in fixtures {
        let language = detect_document_language(&fixture.text);
        assert!(
            !language.language.trim().is_empty(),
            "language detector returned an empty tag for {}",
            fixture.language
        );

        let date = extract_issue_date_suggestion(&fixture.text, &language)
            .unwrap_or_else(|| panic!("no date candidate for {}", fixture.language));
        let date = validate_document_date_suggestion(date, 0.55).unwrap_or_else(|errors| {
            panic!("date rejected for {}: {:?}", fixture.language, errors)
        });
        assert_eq!(
            date.date, fixture.expected_document_date,
            "date eval failed for {}",
            fixture.language
        );

        let correspondent = validate_choice_suggestion(
            ChoiceSuggestion {
                name: fixture.expected_correspondent.clone(),
                confidence: Some(0.95),
                evidence: Some(fixture.expected_correspondent.clone()),
            },
            &allowed_correspondents,
            0.65,
        )
        .unwrap_or_else(|errors| {
            panic!(
                "correspondent fixture rejected for {}: {:?}",
                fixture.language, errors
            )
        });
        assert_eq!(correspondent.name, fixture.expected_correspondent);

        let document_type = validate_choice_suggestion(
            ChoiceSuggestion {
                name: fixture.expected_document_type.clone(),
                confidence: Some(0.95),
                evidence: Some(fixture.expected_document_type.clone()),
            },
            &allowed_document_types,
            0.65,
        )
        .unwrap_or_else(|errors| {
            panic!(
                "document type fixture rejected for {}: {:?}",
                fixture.language, errors
            )
        });
        assert_eq!(document_type.name, fixture.expected_document_type);
        evaluated += 1;
    }

    assert!(evaluated >= 15, "expected a broad multilingual fixture set");
}

#[test]
fn standard_metadata_eval_rejects_ambiguous_or_unknown_values() {
    let language = detect_document_language(
        "Supplier Example\nPayment due: 2026-04-30\nScanned on: 2026-05-01",
    );
    let suggestion = extract_issue_date_suggestion(
        "Supplier Example\nPayment due: 2026-04-30\nScanned on: 2026-05-01",
        &language,
    )
    .expect("candidate should be found");
    let errors = validate_document_date_suggestion(suggestion, 0.7).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("confidence"))
    );

    let errors = validate_choice_suggestion(
        ChoiceSuggestion {
            name: "Unknown Supplier".to_owned(),
            confidence: Some(0.98),
            evidence: Some("Unknown Supplier".to_owned()),
        },
        &["Known Supplier".to_owned()],
        0.65,
    )
    .unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.to_string().contains("unknown"))
    );
}
