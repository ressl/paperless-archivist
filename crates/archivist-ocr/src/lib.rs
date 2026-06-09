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
/// Per-page ceiling on a rendered PNG. A page with a pathologically large
/// MediaBox renders to a huge raster at 120 DPI; the file size is checked from
/// metadata before it is read into memory, so an oversize page is rejected
/// rather than buffered. A normal A4 page at 120 DPI is well under 5 MB.
const MAX_RENDERED_PAGE_BYTES: u64 = 50 * 1024 * 1024;
/// Ceiling on the total rendered bytes held in memory across all pages of one
/// document, bounding peak memory under concurrent OCR jobs.
const MAX_RENDERED_TOTAL_BYTES: u64 = 200 * 1024 * 1024;

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
    let mut total_bytes: u64 = 0;
    let mut entries = tokio::fs::read_dir(tempdir.path())
        .await
        .context("read OCR temp directory")?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("png") {
            // Check the size from metadata BEFORE reading, so an oversize
            // raster is rejected instead of loaded into memory.
            let len = entry.metadata().await?.len();
            total_bytes = accumulate_rendered_page_size(total_bytes, len)?;
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

/// Enforce the per-page and running-total rendered-byte ceilings, returning
/// the updated running total or an error naming the breached cap. Pulled out
/// of the render loop so the bounds logic is unit-testable without rendering a
/// pathological PDF. #256.
fn accumulate_rendered_page_size(running_total: u64, page_len: u64) -> Result<u64> {
    if page_len > MAX_RENDERED_PAGE_BYTES {
        return Err(anyhow!(
            "rendered OCR page is {page_len} bytes, exceeding the {MAX_RENDERED_PAGE_BYTES}-byte per-page cap"
        ));
    }
    let total = running_total.saturating_add(page_len);
    if total > MAX_RENDERED_TOTAL_BYTES {
        return Err(anyhow!(
            "rendered OCR pages exceed the {MAX_RENDERED_TOTAL_BYTES}-byte total cap"
        ));
    }
    Ok(total)
}

pub fn normalize_ocr_pages(page_texts: &[String]) -> String {
    page_texts
        .iter()
        .map(|page| strip_code_fences(page))
        .map(|page| page.trim().to_owned())
        .filter(|page| !page.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Returns true when `line` is a standalone markdown code-fence line, i.e. it
/// trims to three backticks optionally followed by an ASCII language tag (the
/// `^\s*```[a-zA-Z]*\s*$` shape). Inline backticks inside running text never
/// match because such lines carry additional non-fence characters.
fn is_code_fence_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("```") && trimmed[3..].chars().all(|c| c.is_ascii_alphabetic())
}

/// Strips leaked markdown code fences from a single OCR page.
///
/// Some vision models wrap their transcription in a ```` ```lang ... ``` ````
/// block or append a stray fence line despite the prompt. Such fences corrupt
/// downstream ingestion, so we remove them here:
///
/// * If the page is wholly wrapped in a triple-backtick block, unwrap it to its
///   inner content.
/// * Otherwise drop any standalone fence lines while keeping every other line
///   (and legitimate inline backticks) verbatim.
pub fn strip_code_fences(page: &str) -> String {
    let trimmed = page.trim();
    let lines: Vec<&str> = trimmed.lines().collect();

    // Whole-page fenced block: unwrap to the inner content.
    if lines.len() >= 2
        && is_code_fence_line(lines[0])
        && is_code_fence_line(lines[lines.len() - 1])
    {
        return lines[1..lines.len() - 1].join("\n").trim().to_owned();
    }

    // Stray standalone fence lines (e.g. a ```markdown trailer): drop just
    // those lines and keep the rest of the page untouched.
    if lines.iter().any(|line| is_code_fence_line(line)) {
        return trimmed
            .lines()
            .filter(|line| !is_code_fence_line(line))
            .collect::<Vec<_>>()
            .join("\n");
    }

    // No fences present: behave as a no-op.
    page.to_owned()
}

pub fn validate_ocr_text(text: &str, min_chars: usize) -> Result<()> {
    if text.trim().chars().count() < min_chars {
        return Err(anyhow!("OCR result is below minimum length"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_code_fences_unwraps_whole_page_block() {
        let page = "```\nInvoice 42\nTotal: 9.90\n```";
        assert_eq!(strip_code_fences(page), "Invoice 42\nTotal: 9.90");
    }

    #[test]
    fn strip_code_fences_unwraps_language_tagged_block() {
        let page = "```markdown\nHello world\n```";
        assert_eq!(strip_code_fences(page), "Hello world");
    }

    #[test]
    fn strip_code_fences_drops_markdown_trailer() {
        let page = "Real OCR line one\nReal OCR line two\n```markdown";
        assert_eq!(
            strip_code_fences(page),
            "Real OCR line one\nReal OCR line two"
        );
    }

    #[test]
    fn strip_code_fences_is_noop_on_clean_text() {
        let page = "Use the `--flag` switch to enable it.\nSecond line.";
        assert_eq!(strip_code_fences(page), page);
    }

    #[test]
    fn rendered_page_size_caps_enforced() {
        // Normal pages accumulate.
        let total = accumulate_rendered_page_size(0, 4 * 1024 * 1024).unwrap();
        assert_eq!(total, 4 * 1024 * 1024);

        // A single oversize page is rejected (per-page cap).
        assert!(accumulate_rendered_page_size(0, MAX_RENDERED_PAGE_BYTES + 1).is_err());

        // Many in-cap pages that together exceed the total cap are rejected.
        assert!(
            accumulate_rendered_page_size(MAX_RENDERED_TOTAL_BYTES, 1).is_err(),
            "running total over the cap must error"
        );
    }
}
