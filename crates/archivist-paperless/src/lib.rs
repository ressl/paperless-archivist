use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_core::DocumentPatch;
use bytes::Bytes;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone)]
pub struct PaperlessClient {
    base_url: Url,
    client: reqwest::Client,
}

impl PaperlessClient {
    pub fn new(base_url: &str, token: SecretString, timeout_seconds: u64) -> Result<Self> {
        let mut base_url = Url::parse(base_url).context("parse Paperless base URL")?;
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path().trim_end_matches('/')));
        }

        let mut headers = HeaderMap::new();
        let auth = format!("Token {}", token.expose_secret());
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth).context("build Paperless auth header")?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_seconds))
            .default_headers(headers)
            .build()
            .context("build Paperless HTTP client")?;

        Ok(Self { base_url, client })
    }

    pub async fn test_connection(&self) -> Result<PaperlessStatus> {
        let url = self.url("api/")?;
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("connect to Paperless")?;
        if !response.status().is_success() {
            return Err(anyhow!("Paperless returned {}", response.status()));
        }
        Ok(PaperlessStatus { ok: true })
    }

    pub async fn list_tags(&self) -> Result<Vec<PaperlessTag>> {
        self.get_paginated("api/tags/").await
    }

    pub async fn list_correspondents(&self) -> Result<Vec<PaperlessNamedEntity>> {
        self.get_paginated("api/correspondents/").await
    }

    pub async fn list_document_types(&self) -> Result<Vec<PaperlessNamedEntity>> {
        self.get_paginated("api/document_types/").await
    }

    pub async fn list_custom_fields(&self) -> Result<Vec<PaperlessCustomField>> {
        self.get_paginated("api/custom_fields/").await
    }

    pub async fn list_documents(&self) -> Result<Vec<PaperlessDocumentSummary>> {
        self.get_paginated("api/documents/").await
    }

    pub async fn get_document(&self, id: i32) -> Result<PaperlessDocumentDetail> {
        let url = self.url(&format!("api/documents/{id}/"))?;
        self.get_json(url).await
    }

    pub async fn download_original(&self, id: i32) -> Result<Bytes> {
        let url = self.url(&format!("api/documents/{id}/download/"))?;
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("download Paperless document")?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("Paperless download returned {status}"));
        }
        response
            .bytes()
            .await
            .context("read Paperless document bytes")
    }

    pub async fn patch_document(
        &self,
        id: i32,
        patch: &DocumentPatch,
    ) -> Result<PaperlessDocumentDetail> {
        if patch.is_empty() {
            return self.get_document(id).await;
        }
        let url = self.url(&format!("api/documents/{id}/"))?;
        let response = self
            .client
            .patch(url)
            .json(patch)
            .send()
            .await
            .context("patch Paperless document")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Paperless patch returned {status}: {body}"));
        }
        response
            .json()
            .await
            .context("decode Paperless patch response")
    }

    pub async fn ensure_tag(&self, name: &str) -> Result<PaperlessTag> {
        let tags = self.list_tags().await?;
        if let Some(tag) = tags
            .into_iter()
            .find(|tag| tag.name.eq_ignore_ascii_case(name))
        {
            return Ok(tag);
        }
        let url = self.url("api/tags/")?;
        let response = self
            .client
            .post(url)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .context("create Paperless tag")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Paperless tag create returned {status}: {body}"));
        }
        response
            .json()
            .await
            .context("decode created Paperless tag")
    }

    pub async fn add_and_remove_tags(
        &self,
        id: i32,
        add_ids: &[i32],
        remove_ids: &[i32],
    ) -> Result<PaperlessDocumentDetail> {
        let current = self.get_document(id).await?;
        let mut tag_ids = current.tags;
        tag_ids.retain(|tag_id| !remove_ids.contains(tag_id));
        for tag_id in add_ids {
            if !tag_ids.contains(tag_id) {
                tag_ids.push(*tag_id);
            }
        }
        tag_ids.sort_unstable();
        tag_ids.dedup();
        self.patch_document(
            id,
            &DocumentPatch {
                content: None,
                title: None,
                tags: Some(tag_ids),
                correspondent: None,
                document_type: None,
                custom_fields: None,
            },
        )
        .await
    }

    async fn get_paginated<T>(&self, path: &str) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut url = self.url(path)?;
        url.query_pairs_mut().append_pair("page_size", "100");
        let mut items = Vec::new();

        loop {
            let page: PaperlessPage<T> = self.get_json(url.clone()).await?;
            items.extend(page.results);
            let Some(next) = page.next else {
                break;
            };
            let next_url = Url::parse(&next)
                .or_else(|_| self.base_url.join(&next))
                .context("parse Paperless next page URL")?;
            if next_url.origin() != self.base_url.origin() {
                return Err(anyhow!("Paperless pagination next URL changed origin"));
            }
            url = next_url;
        }

        Ok(items)
    }

    async fn get_json<T>(&self, url: Url) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("Paperless GET request")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Paperless GET returned {status}: {body}"));
        }
        response.json().await.context("decode Paperless JSON")
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base_url
            .join(path.trim_start_matches('/'))
            .context("build Paperless URL")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperlessStatus {
    pub ok: bool,
}

#[derive(Debug, Deserialize)]
struct PaperlessPage<T> {
    #[allow(dead_code)]
    count: usize,
    next: Option<String>,
    #[allow(dead_code)]
    previous: Option<String>,
    results: Vec<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperlessTag {
    pub id: i32,
    pub name: String,
    pub slug: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperlessNamedEntity {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperlessCustomField {
    pub id: i32,
    pub name: String,
    #[serde(default, alias = "data_type")]
    pub data_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperlessDocumentSummary {
    pub id: i32,
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tags: Vec<i32>,
    pub correspondent: Option<i32>,
    pub document_type: Option<i32>,
    #[serde(default, alias = "original_file_name")]
    pub original_file_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperlessDocumentDetail {
    pub id: i32,
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tags: Vec<i32>,
    pub correspondent: Option<i32>,
    pub document_type: Option<i32>,
    #[serde(default, alias = "original_file_name")]
    pub original_file_name: Option<String>,
}
