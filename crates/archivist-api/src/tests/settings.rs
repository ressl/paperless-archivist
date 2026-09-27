//! Settings update preflight and provider secret binding tests.

use crate::*;

#[test]
fn provider_secret_names_are_canonicalized_before_persistence() {
    let settings = RuntimeSettings::default();
    let secrets = HashMap::from([(" OLLAMA ".to_owned(), "secret".to_owned())]);

    let canonical = canonicalize_provider_secrets(&settings, secrets)
        .expect("known provider name should canonicalize");

    assert_eq!(canonical.get("ollama").map(String::as_str), Some("secret"));
    assert_eq!(canonical.len(), 1);
}

#[test]
fn provider_secret_names_reject_unknown_and_duplicate_targets() {
    let settings = RuntimeSettings::default();
    let unknown = canonicalize_provider_secrets(
        &settings,
        HashMap::from([("missing".to_owned(), "secret".to_owned())]),
    )
    .expect_err("unknown secret target must fail before any write");
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    assert!(unknown.message.contains("missing"));

    let duplicate = canonicalize_provider_secrets(
        &settings,
        HashMap::from([
            ("ollama".to_owned(), "first".to_owned()),
            (" OLLAMA ".to_owned(), "second".to_owned()),
        ]),
    )
    .expect_err("two inputs resolving to one provider must fail atomically");
    assert_eq!(duplicate.status, StatusCode::BAD_REQUEST);
    assert!(duplicate.message.contains("ollama"));
}

#[test]
fn settings_update_preflight_rejects_before_secret_mapping_changes() {
    let mut settings = RuntimeSettings::default();
    let mut duplicate = settings.ai.providers[0].clone();
    duplicate.name = format!(" {} ", duplicate.name.to_uppercase());
    duplicate.secret_id = Some(Uuid::new_v4());
    settings.ai.providers.push(duplicate);
    let original_secret_ids = settings
        .ai
        .providers
        .iter()
        .map(|provider| provider.secret_id)
        .collect::<Vec<_>>();
    let submitted_secrets = HashMap::from([("ollama".to_owned(), "new-secret".to_owned())]);
    let mut request = UpdateSettingsRequest {
        settings,
        paperless_token: Some("paperless-secret".to_owned()),
        notification_webhook_url: Some("https://hooks.example.test".to_owned()),
        provider_secrets: Some(submitted_secrets.clone()),
    };

    let error = prepare_settings_update(&mut request)
        .expect_err("duplicate provider names must stop the save preflight");

    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(request.provider_secrets.as_ref(), Some(&submitted_secrets));
    assert_eq!(
        request
            .settings
            .ai
            .providers
            .iter()
            .map(|provider| provider.secret_id)
            .collect::<Vec<_>>(),
        original_secret_ids
    );
}

#[test]
fn settings_update_preflight_validates_defaults_added_by_normalization() {
    let mut settings = RuntimeSettings::default();
    let custom = AiProviderSettings {
        name: "custom".to_owned(),
        kind: AiProviderKind::OpenaiCompatible,
        base_url: "https://custom.example.test/v1".to_owned(),
        default_text_model: Some("custom-model".to_owned()),
        default_vision_model: None,
        cost_per_1m_input_tokens_usd: None,
        cost_per_1m_output_tokens_usd: None,
        secret_id: None,
        enabled: true,
        tuning: ProviderTuning::default(),
    };
    settings.ai.providers = vec![custom];
    settings.ai.default_provider = "custom".to_owned();
    settings.ai.ollama_base_url = "   ".to_owned();
    let mut request = UpdateSettingsRequest {
        settings,
        paperless_token: None,
        notification_webhook_url: None,
        provider_secrets: None,
    };

    let error = prepare_settings_update(&mut request)
        .expect_err("newly appended enabled defaults must be part of save validation");

    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("ollama"));
    assert!(error.message.contains("base URL"));
}

#[test]
fn default_provider_rejects_empty_legacy_base_url() {
    let mut settings = RuntimeSettings::default();
    settings.ai.ollama_base_url = "  ".to_owned();

    let error = provider_for_default_text(&settings)
        .expect_err("corrupt legacy settings must not fall back to localhost");

    assert!(error.to_string().contains("empty base URL"));
    assert!(error.to_string().contains("ollama"));
}
