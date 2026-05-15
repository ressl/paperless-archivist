use archivist_core::detect_document_language;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct LanguageFixture {
    language: String,
    text: String,
}

#[test]
fn fixture_languages_are_detected() {
    let fixtures: Vec<LanguageFixture> =
        serde_json::from_str(include_str!("fixtures/language_samples.json")).unwrap();

    for fixture in fixtures {
        let detected = detect_document_language(&fixture.text);
        assert_eq!(detected.language, fixture.language, "{}", fixture.text);
        assert!(detected.confidence >= 0.35, "{}", fixture.language);
    }
}
