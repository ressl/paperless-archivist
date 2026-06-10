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
///
/// Calibration (#283): the in-flight vision call holds, for one page, ~4.7x its
/// bytes (the page, a fallback clone, the base64 string, the serialized request
/// body). With the download cap + total render cap, the worst-case per-job peak
/// is roughly `download + total_render + 4.7*page`. For the default 1 GiB / 2
/// concurrent workers (~384 MiB budget per job after baseline) these caps keep
/// a single job's peak around ~330 MiB. Raise the worker memory limit (or lower
/// `ARCHIVIST_WORKER_CONCURRENCY`) before raising these.
const MAX_RENDERED_PAGE_BYTES: u64 = 24 * 1024 * 1024;
/// Ceiling on the total rendered bytes held in memory across all pages of one
/// document, bounding peak memory under concurrent OCR jobs.
const MAX_RENDERED_TOTAL_BYTES: u64 = 96 * 1024 * 1024;
/// Defense-in-depth ceiling on the rendered raster's long side, in pixels. A
/// pathological PDF can declare an enormous MediaBox that rasterizes to a
/// multi-gigabyte PNG at `PDF_RENDER_DPI` and fills `/tmp` *before* the
/// post-render byte cap (which checks file metadata) can reject it. We probe
/// the page geometry with `pdfinfo` and reject up front when it would exceed
/// this. The largest ISO page, A0, is ~5616 px on the long side at 120 DPI, so
/// 10000 clears every standard format with margin while still catching the
/// runaway. The k8s `/tmp` `sizeLimit` is the infra-level backstop. #297
const MAX_RENDER_PIXELS_LONG_SIDE: u64 = 10_000;

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
        // Image inputs go straight to the vision model without rendering, but
        // they still get loaded into memory (and base64-encoded), so the
        // per-page byte cap must apply here too — the PDF path is not the only
        // way to feed a huge raster into the worker. #282
        accumulate_rendered_page_size(0, document_bytes.len() as u64)?;
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

    // Disk-fill guard: reject a pathological MediaBox before pdftoppm renders
    // it to a giant raster. #297
    reject_oversized_pages(&input).await?;

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

/// Probe page geometry with `pdfinfo` and reject the document if any page would
/// rasterize beyond [`MAX_RENDER_PIXELS_LONG_SIDE`] at [`PDF_RENDER_DPI`]. This
/// stops a giant-MediaBox disk-fill bomb before pdftoppm writes the raster. If
/// `pdfinfo` is unavailable or can't parse the file (e.g. encrypted/corrupt),
/// we let pdftoppm surface the real error rather than blocking here. #297
async fn reject_oversized_pages(input: &Path) -> Result<()> {
    let output = match Command::new("pdfinfo").arg(input).output().await {
        Ok(output) => output,
        // pdfinfo missing from the image: skip the pre-check rather than fail
        // every render; the byte cap + /tmp sizeLimit still bound the blast.
        Err(error) => {
            warn!(%error, "pdfinfo unavailable; skipping render pixel pre-check");
            return Ok(());
        }
    };
    if !output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(long_side_pts) = pdfinfo_max_page_long_side_pts(&stdout) else {
        return Ok(());
    };
    let long_side_px = (long_side_pts / 72.0 * f64::from(PDF_RENDER_DPI)) as u64;
    if long_side_px > MAX_RENDER_PIXELS_LONG_SIDE {
        return Err(anyhow!(
            "PDF page is {long_side_px}px on the long side at {PDF_RENDER_DPI} DPI, exceeding the {MAX_RENDER_PIXELS_LONG_SIDE}px render cap"
        ));
    }
    Ok(())
}

/// Parse the largest page long-side (in PDF points) from `pdfinfo` output.
/// Handles the single-page `Page size:` line and the per-page `Page  N size:`
/// lines that newer pdfinfo prints for documents with varying page sizes.
fn pdfinfo_max_page_long_side_pts(info: &str) -> Option<f64> {
    let mut max_long: Option<f64> = None;
    for line in info.lines() {
        let trimmed = line.trim_start();
        // Match both "Page size:" and "Page    3 size:".
        if !(trimmed.starts_with("Page size:")
            || (trimmed.starts_with("Page ") && trimmed.contains("size:")))
        {
            continue;
        }
        let Some(after) = trimmed.split("size:").nth(1) else {
            continue;
        };
        // "   595.32 x 841.92 pts (A4)" -> width, "x", height, ...
        let mut tokens = after.split_whitespace();
        let Some(width) = tokens.next().and_then(|value| value.parse::<f64>().ok()) else {
            continue;
        };
        let _ = tokens.next(); // the literal "x"
        let Some(height) = tokens.next().and_then(|value| value.parse::<f64>().ok()) else {
            continue;
        };
        let long = width.max(height);
        max_long = Some(max_long.map_or(long, |current| current.max(long)));
    }
    max_long
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

    #[tokio::test]
    async fn image_input_is_rejected_over_the_per_page_cap() {
        // A small image passes through without rendering.
        let small = vec![0u8; 1024];
        let pages = render_document_pages(&small, Some("scan.png"), 4)
            .await
            .expect("small image ok");
        assert_eq!(pages.len(), 1);

        // An oversize image is rejected by the per-page cap (not buffered onward).
        let huge = vec![0u8; (MAX_RENDERED_PAGE_BYTES + 1) as usize];
        assert!(
            render_document_pages(&huge, Some("scan.jpg"), 4)
                .await
                .is_err()
        );
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

    #[test]
    fn pdfinfo_page_size_parses_single_and_multi_page() {
        // Single "Page size:" line (uniform pages).
        let a4 =
            "Title:          test\nPages:          3\nPage size:      595.32 x 841.92 pts (A4)\n";
        let long = pdfinfo_max_page_long_side_pts(a4).expect("A4 parsed");
        assert!((long - 841.92).abs() < 0.01);

        // Per-page lines with varying sizes: the largest long side wins.
        let varying =
            "Page    1 size: 595.32 x 841.92 pts (A4)\nPage    2 size: 1190.0 x 1684.0 pts\n";
        let long = pdfinfo_max_page_long_side_pts(varying).expect("varying parsed");
        assert!((long - 1684.0).abs() < 0.01);

        // No page-size line -> None (we skip the pre-check rather than guess).
        assert!(pdfinfo_max_page_long_side_pts("Pages: 0\n").is_none());
    }

    #[test]
    fn render_pixel_cap_threshold_matches_a0_and_rejects_runaway() {
        let px = |pts: f64| (pts / 72.0 * f64::from(PDF_RENDER_DPI)) as u64;
        // A0 (3370.4 pts long side) is the largest ISO format; it must pass.
        assert!(px(3370.4) <= MAX_RENDER_PIXELS_LONG_SIDE, "A0 must render");
        // A 40000 pt MediaBox (~55 in) is a runaway and must trip the cap.
        assert!(
            px(40_000.0) > MAX_RENDER_PIXELS_LONG_SIDE,
            "runaway must trip"
        );
    }
}
