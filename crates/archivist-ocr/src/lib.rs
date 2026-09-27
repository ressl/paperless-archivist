mod markup;

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

    // #405: Paperless' `/download/` endpoint serves the *archive* PDF whenever
    // one exists, while `original_name` describes the original upload (.jpg,
    // .tiff, .docx, .eml, ...). Decide by the bytes we actually hold; the
    // file name is only a fallback when the content is not recognisable.
    let kind = match sniff_input_kind(document_bytes) {
        Some(kind) => kind,
        None => input_kind_from_name(original_name)?,
    };

    match kind {
        InputKind::Image(mime_type) => {
            // Image inputs go straight to the vision model without rendering,
            // but they still get loaded into memory (and base64-encoded), so
            // the per-page byte cap must apply here too — the PDF path is not
            // the only way to feed a huge raster into the worker. #282
            accumulate_rendered_page_size(0, document_bytes.len() as u64)?;
            Ok(vec![RenderedPage {
                path: PathBuf::from(original_name.unwrap_or("document-image")),
                mime_type: mime_type.to_owned(),
                bytes: document_bytes.to_vec(),
            }])
        }
        InputKind::Tiff => Err(anyhow!(
            "OCR input is a TIFF image without a Paperless archive PDF; vision models do not accept TIFF"
        )),
        InputKind::Pdf => render_pdf_with_pdftoppm(document_bytes, page_limit).await,
    }
}

/// What the OCR input bytes are. #405
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Pdf,
    /// A raster the vision model accepts directly, with its MIME type.
    Image(&'static str),
    Tiff,
}

/// Identify the input from its magic bytes. `None` when unrecognised. #405
fn sniff_input_kind(bytes: &[u8]) -> Option<InputKind> {
    // PDF readers accept up to 1 KiB of leading garbage before the header.
    let head = &bytes[..bytes.len().min(1024)];
    if head.windows(5).any(|window| window == b"%PDF-") {
        return Some(InputKind::Pdf);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(InputKind::Image("image/png"));
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(InputKind::Image("image/jpeg"));
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(InputKind::Image("image/webp"));
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some(InputKind::Tiff);
    }
    None
}

/// Fallback classification from the file name (missing name = PDF, the
/// historical default). #405
fn input_kind_from_name(original_name: Option<&str>) -> Result<InputKind> {
    let extension = original_name
        .and_then(|name| Path::new(name).extension())
        .and_then(|ext| ext.to_str())
        .unwrap_or("pdf")
        .to_ascii_lowercase();
    match extension.as_str() {
        "pdf" => Ok(InputKind::Pdf),
        "png" => Ok(InputKind::Image("image/png")),
        "jpg" | "jpeg" => Ok(InputKind::Image("image/jpeg")),
        "webp" => Ok(InputKind::Image("image/webp")),
        "tif" | "tiff" => Ok(InputKind::Tiff),
        _ => Err(anyhow!(
            "OCR rendering supports PDF and image inputs for MVP, got .{extension}"
        )),
    }
}

/// Wall-clock budget for the `pdfinfo` geometry probe. #407
const PDFINFO_TIMEOUT: Duration = Duration::from_secs(30);

/// Run `command` to completion with a wall-clock bound. The child is spawned
/// with `kill_on_drop`, so on timeout it is killed rather than left running
/// (a hung poppler process used to block the whole job batch). Returns
/// `Ok(None)` on timeout. #407
async fn output_with_timeout(
    command: &mut Command,
    budget: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    let child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(budget, child.wait_with_output()).await {
        Ok(output) => output.map(Some),
        // Dropping the `wait_with_output` future drops the child -> SIGKILL.
        Err(_) => Ok(None),
    }
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
    reject_oversized_pages(&input, page_limit).await?;

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

/// Probe page geometry with `pdfinfo` and reject the document if any page in
/// the rendered range (`1..=page_limit`) would rasterize beyond
/// [`MAX_RENDER_PIXELS_LONG_SIDE`] at [`PDF_RENDER_DPI`]. This stops a
/// giant-MediaBox disk-fill bomb before pdftoppm writes the raster. If
/// `pdfinfo` is unavailable or can't parse the file (e.g. encrypted/corrupt),
/// we let pdftoppm surface the real error rather than blocking here. #297
async fn reject_oversized_pages(input: &Path, page_limit: u16) -> Result<()> {
    // Without -f/-l pdfinfo reports only page 1, so an oversized page 2..N
    // slipped past the cap even though pdftoppm still rendered it. Probe
    // exactly the range pdftoppm will rasterize; pdfinfo clamps -l beyond the
    // page count and the parser handles the per-page `Page N size:` lines. #309
    let probe = output_with_timeout(
        Command::new("pdfinfo")
            .arg("-f")
            .arg("1")
            .arg("-l")
            .arg(page_limit.to_string())
            .arg(input),
        PDFINFO_TIMEOUT,
    )
    .await;
    let output = match probe {
        Ok(Some(output)) => output,
        // A corrupt PDF can hang poppler; the child is already killed. #407
        Ok(None) => {
            return Err(anyhow!(
                "pdfinfo timed out after {PDFINFO_TIMEOUT:?} probing the PDF"
            ));
        }
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
    markup::normalize_pages(page_texts).text
}

pub fn normalize_and_validate_ocr_pages(page_texts: &[String], min_chars: usize) -> Result<String> {
    let normalized = markup::normalize_pages(page_texts);
    if normalized.normalization_limits_exceeded {
        return Err(anyhow!("OCR layout markup exceeded normalization limits"));
    }
    if normalized.had_layout_markup && normalized.text.trim().is_empty() {
        return Err(anyhow!(
            "OCR produced no text after layout markup normalization"
        ));
    }
    validate_ocr_text(&normalized.text, min_chars)?;
    Ok(normalized.text)
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
    fn normalize_ocr_pages_preserves_raw_comparisons_inside_markup() {
        let pages = vec!["<p>Betrag < 100 EUR faellig; 1 < 2 und x > y.</p>".to_owned()];
        assert_eq!(
            normalize_ocr_pages(&pages),
            "Betrag < 100 EUR faellig; 1 < 2 und x > y."
        );
    }

    #[test]
    fn normalize_ocr_pages_keeps_literal_plain_text_tags() {
        for page in [
            "A < B and <document> is printed text.",
            "Der Begriff <brutto> ist kein HTML.",
            "Use the literal <p> tag in documentation.",
        ] {
            assert_eq!(normalize_ocr_pages(&[page.to_owned()]), page);
        }
    }

    #[test]
    fn normalize_ocr_pages_requires_real_balanced_closing_evidence() {
        for page in [
            "<p>literal</paragraph>",
            r#"<p data-note="</p>">literal"#,
            "Prefix <p>one</p><!-- </div> --><div>literal",
            "Prefix <p>one<p>two</p>",
            "</p></div><p>one<div>two",
            "<p>literal <div>closed</div>",
        ] {
            assert_eq!(normalize_ocr_pages(&[page.to_owned()]), page);
        }
    }

    #[test]
    fn normalize_ocr_pages_ignores_layout_tokens_inside_raw_text() {
        for page in [
            "<script><table><tr><td>hidden</td></tr></table></script>",
            "<style>.x::before { content: '<table>'; }</style>",
            "<title><table></title>",
            "<textarea><table></textarea>",
            "<xmp><table></xmp>",
            "<iframe><table></iframe>",
            "<noembed><table></noembed>",
            "<noframes><table></noframes>",
            "<noscript><table></noscript>",
            "<plaintext><table>",
        ] {
            assert_eq!(normalize_ocr_pages(&[page.to_owned()]), page);
        }
    }

    #[test]
    fn normalize_ocr_pages_recognizes_uppercase_and_marker_free_layout() {
        let pages = vec![
            "<DIV><H1>Rechnung</H1><UL><LI>Position eins</LI><LI>Position zwei</LI></UL></DIV>"
                .to_owned(),
        ];
        assert_eq!(
            normalize_ocr_pages(&pages),
            "Rechnung\nPosition eins\nPosition zwei"
        );
    }

    #[test]
    fn normalize_ocr_pages_handles_quoted_angles_comments_and_hidden_subtrees() {
        let pages = vec![
            r#"<div data-bbox="1 2 3 4" data-note="x > y"><!-- > ignored --><style>p { color: red; }</style><script>alert(1)</script><svg><text>vector</text></svg><noscript>fallback</noscript><p>Rechnung 4711</p></div>"#
                .to_owned(),
        ];
        assert_eq!(normalize_ocr_pages(&pages), "Rechnung 4711");
    }

    #[test]
    fn normalize_ocr_pages_drops_document_head_content() {
        let pages = vec![
            r#"<html><head><title>metadata</title></head><body><div data-bbox="1 2 3 4"><p>Rechnung 4711</p></div></body></html>"#
                .to_owned(),
        ];
        assert_eq!(normalize_ocr_pages(&pages), "Rechnung 4711");
    }

    #[test]
    fn normalize_ocr_pages_decodes_safe_entities_without_controls() {
        let pages = vec![
            "<p>Caf&eacute; &agrave; Gen&egrave;ve, 25&deg;C, &euro;50; &#0; &#x1f;</p>".to_owned(),
        ];
        let normalized = normalize_ocr_pages(&pages);
        assert!(normalized.starts_with("Café à Genève, 25°C, €50;"));
        assert!(
            !normalized
                .chars()
                .any(|ch| ch == '\0' || (ch.is_control() && ch != '\n' && ch != '\t'))
        );
    }

    #[test]
    fn normalize_ocr_pages_decodes_entity_after_bare_ampersand() {
        let pages = vec!["<p>Firma M & M&uuml;ller; R&D</p>".to_owned()];
        assert_eq!(normalize_ocr_pages(&pages), "Firma M & Müller; R&D");
    }

    #[test]
    fn normalize_ocr_pages_leaves_plain_text_entities_unchanged() {
        let page = "Printed HTML example: Gr&ouml;&szlig;e &amp; value";
        assert_eq!(normalize_ocr_pages(&[page.to_owned()]), page);
    }

    #[test]
    fn normalize_ocr_pages_preserves_pretty_table_cells() {
        let pages = vec!["<table>\n<tr>\n<td>Name</td>\n<td>42</td>\n</tr>\n</table>".to_owned()];
        assert_eq!(normalize_ocr_pages(&pages), "Name | 42");
    }

    #[test]
    fn normalize_ocr_pages_preserves_empty_table_columns() {
        let pages = vec![
            "<table><tr><td>Name</td><td></td><td>42</td></tr><tr><td></td><td>7</td><td></td></tr></table>"
                .to_owned(),
        ];
        assert_eq!(normalize_ocr_pages(&pages), "Name | | 42\n| 7 |");
    }

    #[test]
    fn normalize_ocr_pages_preserves_fully_empty_table_rows() {
        let pages = vec![
            "<table><tr><td></td></tr><tr><td></td><td></td></tr><tr><td>kept</td></tr></table>"
                .to_owned(),
        ];
        assert_eq!(normalize_ocr_pages(&pages), "|\n| |\nkept");
    }

    #[test]
    fn normalize_ocr_pages_separates_blocks_and_implicit_list_items() {
        let pages = vec![
            "<div><h1>Heading</h1><ul><li>One<li>Two</ul><blockquote>Quote</blockquote><p>Tail</p></div>"
                .to_owned(),
        ];
        assert_eq!(
            normalize_ocr_pages(&pages),
            "Heading\nOne\nTwo\nQuote\nTail"
        );
    }

    #[test]
    fn normalize_ocr_pages_strips_fences_revealed_by_markup() {
        let pages = vec![
            "<p>Invoice 42</p>\n<p>```markdown</p>".to_owned(),
            "<p>&#96;&#96;&#96;</p>".to_owned(),
        ];
        assert_eq!(normalize_ocr_pages(&pages), "Invoice 42");
    }

    #[test]
    fn normalize_ocr_pages_is_idempotent_after_markup_is_removed() {
        let once = normalize_ocr_pages(&["<p>M&uuml;nchen</p>".to_owned()]);
        let twice = normalize_ocr_pages(std::slice::from_ref(&once));
        assert_eq!(twice, once);
    }

    #[test]
    fn normalize_and_validate_reports_layout_only_documents() {
        let pages = vec![r#"<div data-bbox="0 0 100 100" data-label="Picture"></div>"#.to_owned()];
        let error = normalize_and_validate_ocr_pages(&pages, 10)
            .expect_err("layout-only OCR must be distinguishable");
        assert_eq!(
            error.to_string(),
            "OCR produced no text after layout markup normalization"
        );
    }

    #[test]
    fn normalize_and_validate_keeps_the_generic_short_text_error_for_plain_text() {
        let error = normalize_and_validate_ocr_pages(&["short".to_owned()], 10)
            .expect_err("short plain text must still fail");
        assert_eq!(error.to_string(), "OCR result is below minimum length");
    }

    #[test]
    fn normalize_and_validate_rejects_pathological_layout_nesting() {
        let deeply_nested = format!(
            "<div data-bbox=\"0 0 1 1\">{}text{}",
            "<span>".repeat(300),
            "</span>".repeat(300)
        );
        assert_eq!(
            normalize_ocr_pages(std::slice::from_ref(&deeply_nested)),
            deeply_nested,
            "the legacy convenience API must not silently drop rejected text"
        );
        let error = normalize_and_validate_ocr_pages(&[deeply_nested], 1)
            .expect_err("pathological nesting must be rejected before DOM traversal");
        assert_eq!(
            error.to_string(),
            "OCR layout markup exceeded normalization limits"
        );
    }

    #[test]
    fn normalize_and_validate_rejects_repeated_unclosed_table_cells() {
        let malformed_table = format!("<table><tr>{}</table>", "<td>cell".repeat(300));
        let error = normalize_and_validate_ocr_pages(&[malformed_table], 1)
            .expect_err("pathological unclosed cells must be rejected before DOM traversal");
        assert_eq!(
            error.to_string(),
            "OCR layout markup exceeded normalization limits"
        );
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

    /// Assemble a minimal two-page PDF (page 1 = A4, page 2 = caller-supplied
    /// MediaBox) with a valid xref so pdfinfo parses it without repair.
    fn two_page_pdf(second_media_box: &str) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] >>".to_owned(),
            format!("<< /Type /Page /Parent 2 0 R /MediaBox [{second_media_box}] >>"),
        ];
        let mut pdf = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{body}\nendobj\n", index + 1));
        }
        let xref_offset = pdf.len();
        pdf.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
        pdf.push_str("0000000000 65535 f \n");
        for offset in offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            objects.len() + 1
        ));
        pdf.into_bytes()
    }

    #[tokio::test]
    async fn oversized_trailing_page_is_rejected_within_the_render_range() {
        // #309: the probe must cover every page pdftoppm will render, not just
        // page 1. Skipped when poppler is not installed (the production guard
        // itself degrades to a no-op in that case, so there is nothing to test).
        if Command::new("pdfinfo").arg("-v").output().await.is_err() {
            eprintln!("pdfinfo not installed; skipping render-range probe test");
            return;
        }
        let tempdir = tempfile::tempdir().expect("tempdir");
        let input = tempdir.path().join("input.pdf");
        // Page 1 is A4; page 2 declares a runaway 40000x40000 pt MediaBox.
        std::fs::write(&input, two_page_pdf("0 0 40000 40000")).expect("write pdf");

        // Page 2 falls inside the rendered range -> the pixel cap must trip.
        let error = reject_oversized_pages(&input, 2)
            .await
            .expect_err("oversized page 2 must be rejected");
        assert!(
            error.to_string().contains("render cap"),
            "unexpected error: {error}"
        );
        // A -l beyond the page count clamps and still sees page 2.
        assert!(reject_oversized_pages(&input, 3).await.is_err());
        // Page 2 is never rendered at page_limit=1 -> nothing to reject.
        reject_oversized_pages(&input, 1)
            .await
            .expect("A4 page 1 alone is within the cap");
    }

    const JPEG_HEAD: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];

    #[test]
    fn input_kind_follows_the_bytes_not_the_original_file_name() {
        // #405: Paperless serves the archive PDF for .jpg/.tiff/.docx/.eml
        // originals; the bytes decide.
        assert_eq!(
            sniff_input_kind(&two_page_pdf("0 0 595 842")),
            Some(InputKind::Pdf)
        );
        // Leading garbage before the header is tolerated like PDF readers do.
        assert_eq!(
            sniff_input_kind(b"\xEF\xBB\xBF\r\n%PDF-1.7\n"),
            Some(InputKind::Pdf)
        );
        assert_eq!(
            sniff_input_kind(JPEG_HEAD),
            Some(InputKind::Image("image/jpeg"))
        );
        assert_eq!(
            sniff_input_kind(b"\x89PNG\r\n\x1a\n...."),
            Some(InputKind::Image("image/png"))
        );
        assert_eq!(
            sniff_input_kind(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some(InputKind::Image("image/webp"))
        );
        assert_eq!(sniff_input_kind(b"II*\0rest"), Some(InputKind::Tiff));
        assert_eq!(sniff_input_kind(b"MM\0*rest"), Some(InputKind::Tiff));
        assert_eq!(sniff_input_kind(&[0u8; 64]), None);
        // Unknown bytes fall back to the name.
        assert_eq!(
            input_kind_from_name(Some("x.JPEG")).unwrap(),
            InputKind::Image("image/jpeg")
        );
        assert_eq!(input_kind_from_name(None).unwrap(), InputKind::Pdf);
        assert!(input_kind_from_name(Some("letter.docx")).is_err());
    }

    #[tokio::test]
    async fn image_mime_type_matches_the_bytes() {
        // #405: a JPEG labelled .png must reach the model as image/jpeg.
        let pages = render_document_pages(JPEG_HEAD, Some("scan.png"), 4)
            .await
            .expect("jpeg bytes accepted");
        assert_eq!(pages[0].mime_type, "image/jpeg");
        // A real TIFF without archive PDF fails with a clear message instead
        // of being mislabelled.
        let error = render_document_pages(b"II*\0....", Some("fax.tiff"), 4)
            .await
            .expect_err("tiff is not a vision input");
        assert!(error.to_string().contains("TIFF"));
    }

    #[tokio::test]
    async fn archive_pdf_of_non_pdf_original_is_rendered_as_pdf() {
        // #405: .jpg/.docx originals with an archive PDF render via pdftoppm.
        if Command::new("pdftoppm").arg("-v").output().await.is_err() {
            eprintln!("pdftoppm not installed; skipping archive-PDF render test");
            return;
        }
        let pdf = two_page_pdf("0 0 595 842");
        for name in ["scan.jpg", "letter.docx", "fax.tiff"] {
            let pages = render_document_pages(&pdf, Some(name), 2)
                .await
                .expect("archive PDF renders");
            assert_eq!(pages.len(), 2, "{name}");
            assert!(pages.iter().all(|page| page.mime_type == "image/png"));
        }
    }

    #[tokio::test]
    async fn hung_subprocess_is_killed_after_its_budget() {
        // #407: a hanging child is bounded and killed, not awaited forever.
        let started = std::time::Instant::now();
        let output =
            output_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(200))
                .await
                .expect("spawn sleep");
        assert!(output.is_none(), "timeout reported");
        assert!(started.elapsed() < Duration::from_secs(5));

        let output = output_with_timeout(&mut Command::new("true"), Duration::from_secs(10))
            .await
            .expect("spawn true")
            .expect("finishes in budget");
        assert!(output.status.success());
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
