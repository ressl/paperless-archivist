use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use archivist_core::{
    ChoiceSuggestion, FieldSuggestion, TagSuggestion, TitleSuggestion, normalize_model_json,
};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiResponse {
    pub provider: String,
    pub model: String,
    pub text: String,
    pub raw_response: Value,
    pub duration_ms: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub system_prompt: String,
    pub user_prompt: String,
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionRequest {
    pub model: String,
    pub prompt: String,
    pub images: Vec<ImageInput>,
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageInput {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[async_trait]
pub trait TextProvider: Send + Sync {
    async fn chat(&self, request: ChatRequest) -> Result<AiResponse>;
}

#[async_trait]
pub trait VisionProvider: Send + Sync {
    async fn vision(&self, request: VisionRequest) -> Result<AiResponse>;
}

#[derive(Clone)]
pub struct OllamaClient {
    base_url: String,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaModelDetails {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub parameter_size: Option<String>,
    #[serde(default)]
    pub quantization_level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaModel {
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub modified_at: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub details: Option<OllamaModelDetails>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<RawOllamaModel>,
}

#[derive(Debug, Deserialize)]
struct RawOllamaModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    modified_at: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    details: Option<OllamaModelDetails>,
}

impl OllamaClient {
    pub fn new(base_url: &str, token: Option<SecretString>) -> Result<Self> {
        Self::new_with_timeout(base_url, token, Duration::from_secs(180))
    }

    pub fn new_with_timeout(
        base_url: &str,
        token: Option<SecretString>,
        timeout: Duration,
    ) -> Result<Self> {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            let value = format!("Bearer {}", token.expose_secret());
            headers.insert(AUTHORIZATION, HeaderValue::from_str(&value)?);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .build()
            .context("build Ollama HTTP client")?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client,
        })
    }

    pub async fn test_connection(&self, model: Option<&str>) -> Result<Value> {
        let url = format!("{}/api/tags", self.base_url);
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("connect to Ollama")?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("Ollama returned {status}"));
        }
        let value: Value = response
            .json()
            .await
            .context("decode Ollama tags response")?;
        if let Some(model) = model {
            let found = value
                .get("models")
                .and_then(Value::as_array)
                .map(|models| {
                    models.iter().any(|entry| {
                        entry.get("name").and_then(Value::as_str) == Some(model)
                            || entry.get("model").and_then(Value::as_str) == Some(model)
                    })
                })
                .unwrap_or(false);
            if !found {
                return Err(anyhow!(
                    "Ollama is reachable but model '{model}' was not listed"
                ));
            }
        }
        Ok(value)
    }

    pub async fn list_models(&self) -> Result<Vec<OllamaModel>> {
        let url = format!("{}/api/tags", self.base_url);
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("connect to Ollama")?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("Ollama returned {status}"));
        }
        let response: OllamaTagsResponse = response
            .json()
            .await
            .context("decode Ollama tags response")?;
        Ok(normalize_ollama_models(response.models))
    }
}

fn normalize_ollama_models(raw_models: Vec<RawOllamaModel>) -> Vec<OllamaModel> {
    let mut models = raw_models
        .into_iter()
        .filter_map(|raw| {
            let fallback_name = raw.model.as_deref().unwrap_or_default();
            let name = raw.name.trim();
            let name = if name.is_empty() {
                fallback_name.trim()
            } else {
                name
            };
            if name.is_empty() {
                return None;
            }
            Some(OllamaModel {
                name: name.to_owned(),
                model: raw.model,
                modified_at: raw.modified_at,
                size: raw.size,
                digest: raw.digest,
                details: raw.details,
            })
        })
        .collect::<Vec<_>>();
    models.sort_by_key(|model| model.name.to_ascii_lowercase());
    models.dedup_by(|left, right| left.name.eq_ignore_ascii_case(&right.name));
    models
}

#[async_trait]
impl TextProvider for OllamaClient {
    async fn chat(&self, request: ChatRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let payload = json!({
            "model": request.model,
            "stream": false,
            "options": { "temperature": request.temperature },
            "messages": [
                { "role": "system", "content": request.system_prompt },
                { "role": "user", "content": request.user_prompt }
            ]
        });
        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("call Ollama chat")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Ollama chat returned {status}: {body}"));
        }
        let raw: Value = response
            .json()
            .await
            .context("decode Ollama chat response")?;
        let text = raw
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(AiResponse {
            provider: "ollama".to_owned(),
            model: request.model,
            text,
            raw_response: raw,
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        })
    }
}

#[async_trait]
impl VisionProvider for OllamaClient {
    async fn vision(&self, request: VisionRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let images: Vec<String> = request
            .images
            .iter()
            .map(|image| BASE64.encode(&image.bytes))
            .collect();
        let payload = json!({
            "model": request.model,
            "stream": false,
            "options": { "temperature": request.temperature },
            "messages": [
                { "role": "user", "content": request.prompt, "images": images }
            ]
        });
        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("call Ollama vision")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Ollama vision returned {status}: {body}"));
        }
        let raw: Value = response
            .json()
            .await
            .context("decode Ollama vision response")?;
        let text = raw
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(AiResponse {
            provider: "ollama".to_owned(),
            model: request.model,
            text,
            raw_response: raw,
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        })
    }
}

#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    base_url: String,
    client: reqwest::Client,
    provider_name: String,
}

impl OpenAiCompatibleClient {
    pub fn new(provider_name: &str, base_url: &str, api_key: Option<SecretString>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        if let Some(api_key) = api_key {
            let value = format!("Bearer {}", api_key.expose_secret());
            headers.insert(AUTHORIZATION, HeaderValue::from_str(&value)?);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(180))
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client,
            provider_name: provider_name.to_owned(),
        })
    }
}

#[async_trait]
impl TextProvider for OpenAiCompatibleClient {
    async fn chat(&self, request: ChatRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let payload = json!({
            "model": request.model,
            "temperature": request.temperature,
            "messages": [
                { "role": "system", "content": request.system_prompt },
                { "role": "user", "content": request.user_prompt }
            ]
        });
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("call OpenAI-compatible chat")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("OpenAI-compatible chat returned {status}: {body}"));
        }
        let raw: Value = response
            .json()
            .await
            .context("decode OpenAI-compatible response")?;
        let text = raw
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(AiResponse {
            provider: self.provider_name.clone(),
            model: request.model,
            text,
            raw_response: raw,
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        })
    }
}

#[async_trait]
impl VisionProvider for OpenAiCompatibleClient {
    async fn vision(&self, request: VisionRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let mut content = vec![json!({ "type": "text", "text": request.prompt })];
        for image in &request.images {
            content.push(json!({
                "type": "image_url",
                "image_url": {
                    "url": format!(
                        "data:{};base64,{}",
                        image.mime_type,
                        BASE64.encode(&image.bytes)
                    )
                }
            }));
        }
        let payload = json!({
            "model": request.model,
            "temperature": request.temperature,
            "messages": [
                { "role": "user", "content": content }
            ]
        });
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("call OpenAI-compatible vision")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenAI-compatible vision returned {status}: {body}"
            ));
        }
        let raw: Value = response
            .json()
            .await
            .context("decode OpenAI-compatible vision response")?;
        let text = raw
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(AiResponse {
            provider: self.provider_name.clone(),
            model: request.model,
            text,
            raw_response: raw,
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        })
    }
}

#[derive(Clone)]
pub struct AnthropicClient {
    base_url: String,
    client: reqwest::Client,
    provider_name: String,
}

impl AnthropicClient {
    pub fn new(provider_name: &str, base_url: &str, api_key: SecretString) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(api_key.expose_secret())?,
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(180))
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client,
            provider_name: provider_name.to_owned(),
        })
    }

    async fn send_messages(
        &self,
        payload: Value,
        model: String,
        started: Instant,
    ) -> Result<AiResponse> {
        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("call Anthropic messages API")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic messages returned {status}: {body}"));
        }
        let raw: Value = response.json().await.context("decode Anthropic response")?;
        let text = raw
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        Ok(AiResponse {
            provider: self.provider_name.clone(),
            model,
            text,
            raw_response: raw,
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        })
    }
}

#[async_trait]
impl TextProvider for AnthropicClient {
    async fn chat(&self, request: ChatRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let payload = json!({
            "model": request.model,
            "max_tokens": 2048,
            "temperature": request.temperature,
            "system": request.system_prompt,
            "messages": [
                { "role": "user", "content": request.user_prompt }
            ]
        });
        self.send_messages(payload, request.model, started).await
    }
}

#[async_trait]
impl VisionProvider for AnthropicClient {
    async fn vision(&self, request: VisionRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let mut content = vec![json!({ "type": "text", "text": request.prompt })];
        for image in &request.images {
            content.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime_type,
                    "data": BASE64.encode(&image.bytes)
                }
            }));
        }
        let payload = json!({
            "model": request.model,
            "max_tokens": 4096,
            "temperature": request.temperature,
            "messages": [
                { "role": "user", "content": content }
            ]
        });
        self.send_messages(payload, request.model, started).await
    }
}

pub fn parse_tag_suggestion(text: &str) -> Result<TagSuggestion> {
    let value =
        normalize_model_json(text).ok_or_else(|| anyhow!("model response did not contain JSON"))?;
    serde_json::from_value(value).context("parse tag suggestion")
}

pub fn parse_title_suggestion(text: &str) -> Result<TitleSuggestion> {
    let value =
        normalize_model_json(text).ok_or_else(|| anyhow!("model response did not contain JSON"))?;
    serde_json::from_value(value).context("parse title suggestion")
}

pub fn parse_choice_suggestion(text: &str) -> Result<ChoiceSuggestion> {
    let value =
        normalize_model_json(text).ok_or_else(|| anyhow!("model response did not contain JSON"))?;
    serde_json::from_value(value).context("parse choice suggestion")
}

pub fn parse_field_suggestion(text: &str) -> Result<FieldSuggestion> {
    let value =
        normalize_model_json(text).ok_or_else(|| anyhow!("model response did not contain JSON"))?;
    serde_json::from_value(value).context("parse field suggestion")
}

pub fn prompt_for_tags(content: &str, allowed_tags: &[String], max_tags: usize) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        temperature: 0.0,
        system_prompt: "You classify Paperless-ngx documents. Return strict JSON only.".to_owned(),
        user_prompt: format!(
            "Allowed tags:\n{}\n\nDocument text:\n{}\n\nReturn JSON: {{\"tags\":[...],\"new_tags\":[],\"confidence\":0.0}}. Select at most {} existing tags.",
            allowed_tags.join("\n"),
            bounded_text(content, 16000),
            max_tags
        ),
    }
}

pub fn prompt_for_title(content: &str) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        temperature: 0.0,
        system_prompt:
            "You generate concise Paperless-ngx document titles. Return strict JSON only."
                .to_owned(),
        user_prompt: format!(
            "Document text:\n{}\n\nReturn JSON: {{\"title\":\"concise human-readable title\",\"confidence\":0.0}}.",
            bounded_text(content, 12000)
        ),
    }
}

pub fn prompt_for_choice(content: &str, choice_kind: &str, allowed: &[String]) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        temperature: 0.0,
        system_prompt: format!(
            "You classify a document by existing {choice_kind}. Return strict JSON only."
        ),
        user_prompt: format!(
            "Allowed values:\n{}\n\nDocument text:\n{}\n\nReturn JSON: {{\"name\":\"one allowed value\",\"confidence\":0.0}}.",
            allowed.join("\n"),
            bounded_text(content, 12000)
        ),
    }
}

pub fn prompt_for_fields(
    content: &str,
    allowed_fields: &[String],
    max_fields: usize,
) -> ChatRequest {
    ChatRequest {
        model: String::new(),
        temperature: 0.0,
        system_prompt: "You extract Paperless-ngx custom field values. Return strict JSON only."
            .to_owned(),
        user_prompt: format!(
            "Allowed custom fields:\n{}\n\nDocument text:\n{}\n\nReturn JSON: {{\"fields\":[{{\"name\":\"allowed field\",\"value\":\"value\",\"confidence\":0.0}}],\"confidence\":0.0}}. Use at most {} fields and only fields with explicit evidence.",
            allowed_fields.join("\n"),
            bounded_text(content, 14000),
            max_fields
        ),
    }
}

fn bounded_text(content: &str, max_chars: usize) -> String {
    content.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tag_json_inside_text() {
        let parsed =
            parse_tag_suggestion("Result: {\"tags\":[\"A\"],\"new_tags\":[],\"confidence\":0.8}")
                .unwrap();
        assert_eq!(parsed.tags, vec!["A"]);
    }

    #[test]
    fn parses_field_json_inside_text() {
        let parsed = parse_field_suggestion(
            "```json\n{\"fields\":[{\"name\":\"Invoice No\",\"value\":\"R-1\",\"confidence\":0.9}],\"confidence\":0.9}\n```",
        )
        .unwrap();
        assert_eq!(parsed.fields[0].name, "Invoice No");
    }

    #[test]
    fn normalizes_ollama_tags_response() {
        let response: OllamaTagsResponse = serde_json::from_value(json!({
            "models": [
                {
                    "model": "zeta:latest",
                    "size": 2147483648_u64,
                    "details": {
                        "parameter_size": "4B",
                        "quantization_level": "Q4_K_M"
                    }
                },
                {
                    "name": "alpha:latest",
                    "model": "alpha:latest",
                    "size": 1073741824_u64,
                    "details": {
                        "parameter_size": "2B",
                        "quantization_level": "Q8_0"
                    }
                },
                {
                    "name": "ALPHA:latest"
                },
                {
                    "name": ""
                }
            ]
        }))
        .expect("valid tags response");

        let models = normalize_ollama_models(response.models);

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "alpha:latest");
        assert_eq!(
            models[0]
                .details
                .as_ref()
                .and_then(|details| details.parameter_size.as_deref()),
            Some("2B")
        );
        assert_eq!(models[1].name, "zeta:latest");
    }
}
