use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tokio::process::Command;
use tracing::{debug, warn};

const PDF_RENDER_DPI: u16 = 120;
/// Base wall-clock budget for a `pdftoppm` invocation.
const PDF_RENDER_BASE_BUDGET: Duration = Duration::from_secs(30);
/// Additional budget granted per requested page to allow large documents to render.
const PDF_RENDER_PER_PAGE_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub path: PathBuf,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

pub async fn render_document_pages(
    document_bytes: &[u8],
    original_name: Option<&str>,
    page_limit: u16,
) -> Result<Vec<RenderedPage>> {
    if page_limit == 0 {
        return Ok(Vec::new());
    }

    let extension = original_name
        .and_then(|name| Path::new(name).extension())
        .and_then(|ext| ext.to_str())
        .unwrap_or("pdf")
        .to_ascii_lowercase();

    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
        let mime_type = mime_guess::from_ext(&extension)
            .first_or_octet_stream()
            .to_string();
        return Ok(vec![RenderedPage {
            path: PathBuf::from(original_name.unwrap_or("document-image")),
            mime_type,
            bytes: document_bytes.to_vec(),
        }]);
    }

    if extension != "pdf" {
        return Err(anyhow!(
            "OCR rendering supports PDF and image inputs for MVP, got .{extension}"
        ));
    }

    render_pdf_with_pdftoppm(document_bytes, page_limit).await
}

async fn render_pdf_with_pdftoppm(
    document_bytes: &[u8],
    page_limit: u16,
) -> Result<Vec<RenderedPage>> {
    let tempdir = tempfile::tempdir().context("create OCR temp directory")?;
    let input = tempdir.path().join("input.pdf");
    let output_prefix = tempdir.path().join("page");
    tokio::fs::write(&input, document_bytes)
        .await
        .context("write OCR input PDF")?;

    let render_budget =
        PDF_RENDER_BASE_BUDGET + PDF_RENDER_PER_PAGE_BUDGET.saturating_mul(u32::from(page_limit));

    let mut child = Command::new("pdftoppm")
        .arg("-png")
        .arg("-r")
        .arg(PDF_RENDER_DPI.to_string())
        .arg("-f")
        .arg("1")
        .arg("-l")
        .arg(page_limit.to_string())
        .arg(&input)
        .arg(&output_prefix)
        .kill_on_drop(true)
        .spawn()
        .context("run pdftoppm; install poppler-utils in the runtime image")?;

    let status = match tokio::time::timeout(render_budget, child.wait()).await {
        Ok(result) => result.context("wait for pdftoppm")?,
        Err(_) => {
            // Budget exceeded (e.g. corrupt/encrypted/bomb PDF). Kill the child so it
            // does not linger as an orphan and surface a transient error for retry.
            if let Err(error) = child.kill().await {
                warn!(%error, "failed to kill pdftoppm after render timeout");
            }
            return Err(anyhow!(
                "pdftoppm exceeded render budget of {render_budget:?}"
            ));
        }
    };

    if !status.success() {
        return Err(anyhow!("pdftoppm exited with {status}"));
    }

    let mut pages = Vec::new();
    let mut entries = tokio::fs::read_dir(tempdir.path())
        .await
        .context("read OCR temp directory")?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("png") {
            let bytes = tokio::fs::read(&path).await?;
            pages.push(RenderedPage {
                path,
                mime_type: "image/png".to_owned(),
                bytes,
            });
        }
    }
    pages.sort_by(|a, b| a.path.cmp(&b.path));
    debug!(
        pages = pages.len(),
        dpi = PDF_RENDER_DPI,
        "rendered PDF pages for OCR"
    );
    Ok(pages)
}

pub fn normalize_ocr_pages(page_texts: &[String]) -> String {
    page_texts
        .iter()
        .map(|page| page.trim())
        .filter(|page| !page.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn validate_ocr_text(text: &str, min_chars: usize) -> Result<()> {
    if text.trim().chars().count() < min_chars {
        return Err(anyhow!("OCR result is below minimum length"));
    }
    Ok(())
}
