use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use archivist_core::DocumentPatch;
use bytes::Bytes;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

/// Typed surface for Paperless-ngx integration failures. The worker uses
/// `is_transient()` to decide whether a job should be retried with backoff
/// or marked permanent — this avoids substring matching on error text. See
/// `archivist-worker::classify_processing_failure`.
#[derive(Debug, Error)]
pub enum PaperlessError {
    /// Network-layer failure: DNS, TCP, TLS or socket close. Always transient.
    #[error("paperless network failure: {0}")]
    Network(String),

    /// Request timed out before Paperless answered. Always transient.
    #[error("paperless request timed out: {0}")]
    Timeout(String),

    /// Paperless returned a 5xx response. Treated as transient.
    #[error("paperless server error: status={status}, body={body}")]
    Server { status: u16, body: String },

    /// Paperless returned a 4xx response. Treated as permanent — a client
    /// fix (auth, payload, missing object) is needed before retrying.
    #[error("paperless client error: status={status}, body={body}")]
    Client { status: u16, body: String },

    /// Response shape or pagination violated an invariant. Permanent.
    #[error("paperless protocol violation: {0}")]
    Protocol(String),
}

impl PaperlessError {
    /// Whether the worker should retry this failure with backoff.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) | Self::Timeout(_) | Self::Server { .. } => true,
            Self::Client { .. } | Self::Protocol(_) => false,
        }
    }

    /// Build a typed error from an unsuccessful HTTP status. Any `5xx` —
    /// including non-standard codes such as Cloudflare 520/521/525 or
    /// 507/508 — is treated as a transient server error so the worker
    /// retries instead of mis-classifying it as permanent.
    fn from_status(status: u16, body: String) -> Self {
        if (500..600).contains(&status) {
            Self::Server { status, body }
        } else {
            Self::Client { status, body }
        }
    }
}

impl From<reqwest::Error> for PaperlessError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout(error.without_url().to_string())
        } else if error.is_connect() || error.is_request() || error.is_body() {
            Self::Network(error.without_url().to_string())
        } else if let Some(status) = error.status() {
            let code = status.as_u16();
            if status.is_server_error() {
                Self::Server {
                    status: code,
                    body: error.without_url().to_string(),
                }
            } else {
                Self::Client {
                    status: code,
                    body: error.without_url().to_string(),
                }
            }
        } else {
            Self::Network(error.without_url().to_string())
        }
    }
}

/// Multiplier applied to the configured (JSON-tuned) request timeout to derive
/// the per-request budget for full-body original downloads. The base timeout is
/// sized for small JSON calls; a large scanned PDF streamed over a slow link
/// needs considerably more headroom before it should be considered transient.
const DOWNLOAD_TIMEOUT_MULTIPLIER: u32 = 10;

#[derive(Clone)]
pub struct PaperlessClient {
    base_url: Url,
    client: reqwest::Client,
    download_timeout: Duration,
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
            // Defense-in-depth against SSRF: never follow redirects. Paperless API
            // calls all use trailing-slash URLs so DRF does not 301 them; refusing
            // redirects stops a 3xx to a loopback/metadata address from being
            // chased after the base URL was validated (#177).
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .context("build Paperless HTTP client")?;

        let download_timeout =
            Duration::from_secs(timeout_seconds.saturating_mul(DOWNLOAD_TIMEOUT_MULTIPLIER as u64));

        Ok(Self {
            base_url,
            client,
            download_timeout,
        })
    }

    pub async fn test_connection(&self) -> Result<PaperlessStatus> {
        let mut url = self.url("api/documents/")?;
        url.query_pairs_mut().append_pair("page_size", "1");
        let target_url = url.to_string();
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("connect to Paperless")?;
        if !response.status().is_success() {
            let status = response.status();
            let hint = if matches!(status.as_u16(), 404..=406) {
                " Check that the Paperless Base URL points to the Paperless-ngx REST service and that the reverse proxy allows API requests with token authentication."
            } else {
                ""
            };
            return Err(anyhow!(
                "Paperless returned {status} for {target_url}.{hint}"
            ));
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

    pub async fn list_documents_modified_since(
        &self,
        since: &str,
    ) -> Result<Vec<PaperlessDocumentSummary>> {
        let mut url = self.url("api/documents/")?;
        url.query_pairs_mut()
            .append_pair("page_size", "100")
            .append_pair("modified__gt", since);
        self.get_paginated_url(url).await
    }

    pub async fn get_document(&self, id: i32) -> Result<PaperlessDocumentDetail> {
        let url = self.url(&format!("api/documents/{id}/"))?;
        self.get_json(url).await
    }

    pub async fn download_original(&self, id: i32) -> Result<Bytes> {
        let url = self.url(&format!("api/documents/{id}/download/"))?;
        // Override the JSON-tuned client timeout with a larger budget so big
        // originals streamed over slow links are not aborted prematurely.
        let response = self
            .client
            .get(url)
            .timeout(self.download_timeout)
            .send()
            .await
            .map_err(PaperlessError::from)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(PaperlessError::from_status(status.as_u16(), body).into());
        }
        response
            .bytes()
            .await
            .map_err(PaperlessError::from)
            .map_err(Into::into)
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
            .map_err(PaperlessError::from)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(PaperlessError::from_status(status.as_u16(), body).into());
        }
        response
            .json()
            .await
            .context("decode Paperless patch response")
    }

    pub async fn ensure_tag(&self, name: &str) -> Result<PaperlessTag> {
        // Unicode-aware case folding so e.g. "Ärzte" and "ärzte" collapse to the
        // same tag instead of creating a near-duplicate in the German catalog.
        let wanted = name.to_lowercase();
        let tags = self.list_tags().await?;
        if let Some(tag) = tags
            .into_iter()
            .find(|tag| tag.name.to_lowercase() == wanted)
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
            .map_err(PaperlessError::from)?;
        let status = response.status();
        if !status.is_success() {
            // A concurrent worker may have created the tag between our list and
            // POST, in which case Paperless rejects the duplicate name with a
            // 4xx. Refetch and reuse the existing tag instead of failing.
            if status.is_client_error()
                && let Some(tag) = self
                    .list_tags()
                    .await?
                    .into_iter()
                    .find(|tag| tag.name.to_lowercase() == wanted)
            {
                return Ok(tag);
            }
            let body = response.text().await.unwrap_or_default();
            return Err(PaperlessError::from_status(status.as_u16(), body).into());
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
                created: None,
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
        self.get_paginated_url(url).await
    }

    async fn get_paginated_url<T>(&self, mut url: Url) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        // Hard cap on pages followed. Paperless paginates at <= 100 items/page,
        // so this tolerates millions of objects while still bounding a server
        // that hands out a cyclic or never-terminating `next` cursor.
        const MAX_PAGES: usize = 100_000;

        let mut items = Vec::new();

        for _ in 0..MAX_PAGES {
            let page: PaperlessPage<T> = self.get_json(url.clone()).await?;
            items.extend(page.results);
            let Some(next) = page.next else {
                return Ok(items);
            };
            let next_url = Url::parse(&next)
                .or_else(|_| self.base_url.join(&next))
                .context("parse Paperless next page URL")?;
            if next_url.origin() != self.base_url.origin() {
                return Err(PaperlessError::Protocol(
                    "Paperless pagination next URL changed origin".to_owned(),
                )
                .into());
            }
            // Guard against a non-advancing cursor that would otherwise loop
            // forever while `items` grows without bound.
            if next_url == url {
                return Err(PaperlessError::Protocol(
                    "Paperless pagination next URL did not advance".to_owned(),
                )
                .into());
            }
            url = next_url;
        }

        Err(
            PaperlessError::Protocol(format!("Paperless pagination exceeded {MAX_PAGES} pages"))
                .into(),
        )
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
            .map_err(PaperlessError::from)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(PaperlessError::from_status(status.as_u16(), body).into());
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
    pub created: Option<String>,
    #[serde(default)]
    pub modified: Option<String>,
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
    pub created: Option<String>,
    #[serde(default)]
    pub modified: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tags: Vec<i32>,
    pub correspondent: Option<i32>,
    pub document_type: Option<i32>,
    #[serde(default, alias = "original_file_name")]
    pub original_file_name: Option<String>,
}
