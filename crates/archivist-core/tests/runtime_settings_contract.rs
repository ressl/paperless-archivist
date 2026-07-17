use archivist_core::RuntimeSettings;
use serde_json::Value;

const FIXTURE: &str = include_str!("../../../openapi/fixtures/runtime-settings.json");
const INPUT_FIXTURE: &str = include_str!("../../../openapi/fixtures/runtime-settings-input.json");

#[test]
fn runtime_settings_fixture_round_trips_without_serde_shape_drift() {
    let expected: Value = serde_json::from_str(FIXTURE).expect("fixture must be valid JSON");
    let settings: RuntimeSettings = serde_json::from_value(expected.clone())
        .expect("fixture must deserialize as RuntimeSettings");
    let actual = serde_json::to_value(settings).expect("RuntimeSettings must serialize");

    assert_same_wire_shape(&actual, &expected, "$");
}

#[test]
fn partial_runtime_settings_input_fixture_matches_serde_defaults() {
    let input: Value = serde_json::from_str(INPUT_FIXTURE).expect("input fixture must be JSON");
    let settings = serde_json::from_value::<RuntimeSettings>(input)
        .expect("input fixture must deserialize through Serde defaults");
    let response = serde_json::to_value(settings).expect("partial input must serialize");

    for branch in [
        "paperless",
        "ai",
        "security",
        "notifications",
        "workflow",
        "ocr",
        "tagging",
        "metadata",
        "fields",
        "ui",
    ] {
        assert!(
            response.get(branch).is_some(),
            "missing response branch {branch}"
        );
    }
    let tuning = &response["ai"]["providers"][0]["tuning"];
    assert_eq!(
        tuning.as_object().map(|object| object.len()),
        Some(21),
        "Serde must serialize every ProviderTuning response member"
    );
}

#[test]
fn skip_serializing_options_are_omitted_from_runtime_settings_response() {
    let mut input: Value = serde_json::from_str(FIXTURE).expect("fixture must be valid JSON");
    input["ai"]["fallback_vision_model"] = Value::Null;
    input["ai"]["consensus_secondary_text_model"] = Value::Null;
    for field in ["label", "usage_tier", "context", "modality", "best_for"] {
        input["ai"]["model_catalog"][0][field] = Value::Null;
    }

    let settings: RuntimeSettings =
        serde_json::from_value(input).expect("nullable input options must deserialize");
    let response = serde_json::to_value(settings).expect("settings must serialize");
    let ai = response["ai"]
        .as_object()
        .expect("AI settings response object");
    assert!(!ai.contains_key("fallback_vision_model"));
    assert!(!ai.contains_key("consensus_secondary_text_model"));
    let catalog = response["ai"]["model_catalog"][0]
        .as_object()
        .expect("model catalog response object");
    for field in ["label", "usage_tier", "context", "modality", "best_for"] {
        assert!(!catalog.contains_key(field), "{field} must be omitted");
    }
}

fn assert_same_wire_shape(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            let actual_keys = actual.keys().collect::<Vec<_>>();
            let expected_keys = expected.keys().collect::<Vec<_>>();
            assert_eq!(actual_keys, expected_keys, "object keys differ at {path}");
            for (key, expected_value) in expected {
                let actual_value = actual.get(key).expect("key sets were compared");
                assert_same_wire_shape(actual_value, expected_value, &format!("{path}.{key}"));
            }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            assert_eq!(
                actual.len(),
                expected.len(),
                "array length differs at {path}"
            );
            for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
                assert_same_wire_shape(actual, expected, &format!("{path}[{index}]"));
            }
        }
        // Serde converts JSON numbers through the Core's f32 settings fields;
        // their decimal representation may change while the wire type remains
        // correct. Semantic range/value behavior is covered by Core tests.
        (Value::Number(_), Value::Number(_)) => {}
        _ => assert_eq!(actual, expected, "value or JSON type differs at {path}"),
    }
}
