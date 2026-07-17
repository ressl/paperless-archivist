# OCR Layout-Markup Normalization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use subagent-driven-development (recommended) or executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert MinerU HTML layout fragments into deterministic archival text without corrupting plain text, table structure, entities, or code-fence invariants.

**Architecture:** Parse candidate pages with Servo `html5ever` and its matching RcDom tree sink, require conservative DOM-backed layout evidence before rewriting, then render the DOM through a project-owned deterministic text renderer. Keep page normalization private, expose one validated document-level API to the worker, and preserve raw page-cache semantics.

**Tech Stack:** Rust 2024, `html5ever` 0.39, `markup5ever_rcdom` 0.39.0+unofficial, existing `anyhow` error handling and Cargo security gates.

## Global Constraints

- Work only in `.worktrees/ocr-markup-hardening` on `codex/ocr-markup-hardening`.
- Do not modify the user-owned dirty OCR file in the root checkout.
- Write and observe failing regression tests before production code.
- Plain-text pages must not decode HTML entities or strip literal tag text.
- Layout-only pages return `OCR produced no text after layout markup normalization`.
- Cache raw provider text after existing fence cleanup; never cache fully normalized layout text.
- Do not activate or deploy MinerU in this change.
- Do not backfill existing OCR hashes.

---

### Task 1: Add the complete failing OCR normalization contract

**Files:**
- Modify: `crates/archivist-ocr/src/lib.rs:321`

**Interfaces:**
- Consumes: existing `normalize_ocr_pages(&[String]) -> String`.
- Produces: regression tests that define the private parser and renderer behavior used by all later tasks.

- [ ] **Step 1: Add parser-safety and detection tests**

Add these tests to `mod tests` in `crates/archivist-ocr/src/lib.rs`:

```rust
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
        r#"<div data-bbox="1 2 3 4" data-note="x > y"><!-- > ignored --><style>p { color: red; }</style><script>alert(1)</script><head>metadata</head><svg><text>vector</text></svg><noscript>fallback</noscript><p>Rechnung 4711</p></div>"#
            .to_owned(),
    ];
    assert_eq!(normalize_ocr_pages(&pages), "Rechnung 4711");
}
```

- [ ] **Step 2: Run parser-safety tests and verify RED**

Run:

```bash
cargo test -p archivist-ocr normalize_ocr_pages_preserves_raw_comparisons_inside_markup --locked
cargo test -p archivist-ocr normalize_ocr_pages_recognizes_uppercase_and_marker_free_layout --locked
cargo test -p archivist-ocr normalize_ocr_pages_handles_quoted_angles_comments_and_hidden_subtrees --locked
```

Expected: each test fails because the current implementation returns raw HTML. Run the literal-plain-text test separately; it should pass as a characterization of behavior that must remain unchanged.

- [ ] **Step 3: Add entity, table, block, and fence-order tests**

```rust
#[test]
fn normalize_ocr_pages_decodes_safe_entities_without_controls() {
    let pages = vec![
        "<p>Caf&eacute; &agrave; Gen&egrave;ve, 25&deg;C, &euro;50; &#0; &#x1f;</p>"
            .to_owned(),
    ];
    let normalized = normalize_ocr_pages(&pages);
    assert!(normalized.starts_with("Café à Genève, 25°C, €50;"));
    assert!(!normalized.chars().any(|ch| ch == '\0' || (ch.is_control() && ch != '\n' && ch != '\t')));
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
    let pages = vec![
        "<table>\n<tr>\n<td>Name</td>\n<td>42</td>\n</tr>\n</table>".to_owned(),
    ];
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
fn normalize_ocr_pages_separates_blocks_and_implicit_list_items() {
    let pages = vec![
        "<div><h1>Heading</h1><ul><li>One<li>Two</ul><blockquote>Quote</blockquote><p>Tail</p></div>"
            .to_owned(),
    ];
    assert_eq!(normalize_ocr_pages(&pages), "Heading\nOne\nTwo\nQuote\nTail");
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
```

- [ ] **Step 4: Run the new behavior tests and verify RED**

Run:

```bash
cargo test -p archivist-ocr normalize_ocr_pages_decodes_safe_entities_without_controls --locked
cargo test -p archivist-ocr normalize_ocr_pages_preserves_pretty_table_cells --locked
cargo test -p archivist-ocr normalize_ocr_pages_strips_fences_revealed_by_markup --locked
```

Expected: FAIL with raw HTML/raw entities. The plain-text entity test should pass as a preserved-behavior characterization.

- [ ] **Step 5: Commit the red contract**

```bash
git add crates/archivist-ocr/src/lib.rs
git commit -m "test(ocr): define layout markup normalization contract"
```

---

### Task 2: Implement tokenizer-backed detection and deterministic rendering

**Files:**
- Modify: `Cargo.toml:20-55`
- Modify: `Cargo.lock`
- Modify: `crates/archivist-ocr/Cargo.toml`
- Create: `crates/archivist-ocr/src/markup.rs`
- Modify: `crates/archivist-ocr/src/lib.rs:1-5,259-267`

**Interfaces:**
- Consumes: regression contract from Task 1 and existing `strip_code_fences`.
- Produces: `markup::normalize_pages(&[String]) -> NormalizedPages` and the unchanged public `normalize_ocr_pages(&[String]) -> String`.

- [ ] **Step 1: Add pinned workspace dependencies**

Add to `[workspace.dependencies]`:

```toml
html5ever = "0.39"
markup5ever_rcdom = "=0.39.0"
```

Add to `crates/archivist-ocr/Cargo.toml`:

```toml
html5ever.workspace = true
markup5ever_rcdom.workspace = true
```

Run `cargo check -p archivist-ocr` once without `--locked` to resolve the new
packages into `Cargo.lock`, then use `--locked` for every subsequent command.

- [ ] **Step 2: Create the parser module with its internal contract**

Create `crates/archivist-ocr/src/markup.rs` with these units:

```rust
use html5ever::{QualName, local_name, ns, parse_document, parse_fragment};
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::strip_code_fences;

pub(crate) struct NormalizedPages {
    pub text: String,
    pub had_layout_markup: bool,
    pub normalization_limits_exceeded: bool,
}

struct NormalizedPage {
    text: String,
    had_layout_markup: bool,
    normalization_limits_exceeded: bool,
}

pub(crate) fn normalize_pages(page_texts: &[String]) -> NormalizedPages {
    let pages = page_texts.iter().map(|page| normalize_page(page)).collect::<Vec<_>>();
    let had_layout_markup = pages.iter().any(|page| page.had_layout_markup);
    let normalization_limits_exceeded = pages
        .iter()
        .any(|page| page.normalization_limits_exceeded);
    let text = pages
        .into_iter()
        .map(|page| page.text.trim().to_owned())
        .filter(|page| !page.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    NormalizedPages {
        text,
        had_layout_markup,
        normalization_limits_exceeded,
    }
}

fn normalize_page(page: &str) -> NormalizedPage {
    let detection_input = strip_code_fences(page);
    let inspection = inspect_markup(&detection_input);
    if !looks_like_layout_markup(&detection_input, &inspection) {
        return NormalizedPage {
            text: detection_input,
            had_layout_markup: false,
            normalization_limits_exceeded: false,
        };
    }
    if inspection.limits_exceeded {
        // The checked worker API turns this into a permanent error. The
        // convenience API retains the source instead of losing text.
        return NormalizedPage {
            text: detection_input,
            had_layout_markup: true,
            normalization_limits_exceeded: true,
        };
    }

    let dom = parse_html(&detection_input);
    let mut rendered = String::with_capacity(detection_input.len());
    render_node(&dom.document, &mut rendered);
    let normalized = normalize_rendered_text(&sanitize_controls(&rendered));
    NormalizedPage {
        text: strip_code_fences(&normalized),
        had_layout_markup: true,
        normalization_limits_exceeded: false,
    }
}

fn parse_html(source: &str) -> RcDom {
    let source_lower = source.to_ascii_lowercase();
    if ["html", "head", "body"]
        .iter()
        .any(|name| contains_start_tag(&source_lower, name))
    {
        parse_document(RcDom::default(), Default::default()).one(source)
    } else {
        parse_fragment(
            RcDom::default(),
            Default::default(),
            QualName::new(None, ns!(html), local_name!("body")),
            Vec::new(),
            false,
        )
        .one(source)
    }
}

fn contains_start_tag(source_lower: &str, name: &str) -> bool {
    let needle = format!("<{name}");
    source_lower.match_indices(&needle).any(|(index, _)| {
        source_lower[index + needle.len()..]
            .chars()
            .next()
            .is_none_or(|next| next.is_ascii_whitespace() || matches!(next, '>' | '/'))
    })
}
```

- [ ] **Step 3: Implement conservative tokenizer-backed detection**

Use one `html5ever::tokenizer::Tokenizer` pass before DOM construction. The
original draft used a raw `source.contains("</tag")` search; review proved that
it accepted tag-name prefixes and closers inside attributes/comments, counted a
single closer multiple times, and rescanned the source per DOM node. That draft
is superseded and must not be implemented.

```rust
struct MarkupInspection {
    marker_attribute: bool,
    table: bool,
    recognized_elements: usize,
    explicitly_closed_elements: usize,
    first_start_tag: Option<String>,
    first_start_tag_matched: bool,
    limits_exceeded: bool,
}

fn inspect_markup(source: &str) -> MarkupInspection {
    // Tokenize once. Count recognized start tags and only matched end-tag
    // events. Track attributes, tables, total tokens, and an explicit stack
    // of open non-void elements. Track whether the exact first start token was
    // matched. Return RawData/Plaintext sink states for HTML raw-text elements
    // so apparent tags in their content are never evidence. Reject above
    // 100,000 tokens or depth 256.
    // See crates/archivist-ocr/src/markup.rs for the complete TokenSink.
}

fn looks_like_layout_markup(source: &str, inspection: &MarkupInspection) -> bool {
    let source_lower = source.to_ascii_lowercase();
    let starts_with_layout = source_lower
        .trim_start()
        .strip_prefix('<')
        .and_then(|rest| {
            let name = rest
                .split(|ch: char| ch.is_ascii_whitespace() || ch == '>' || ch == '/')
                .next()
                .unwrap_or_default();
            is_layout_element(name).then_some(name)
        })
        .is_some_and(|name| {
            inspection.first_start_tag.as_deref() == Some(name)
                && inspection.first_start_tag_matched
        });

    inspection.marker_attribute
        || inspection.table
        || (starts_with_layout && inspection.explicitly_closed_elements >= 1)
        || (inspection.recognized_elements >= 2 && inspection.explicitly_closed_elements >= 2)
}

fn is_layout_element(name: &str) -> bool {
    matches!(
        name,
        "p" | "div" | "section" | "article" | "header" | "footer"
            | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th"
            | "ul" | "ol" | "li" | "blockquote"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
    )
}
```

- [ ] **Step 4: Implement safe DOM traversal and table rendering**

Implement `render_node`, `render_element`, `render_table`, and row collection.
The exact behavior is:

```rust
fn render_node(node: &Handle, output: &mut String) {
    match &node.data {
        NodeData::Text { contents } => output.push_str(&contents.borrow()),
        NodeData::Element { name, .. } => render_element(node, name.local.as_ref(), output),
        NodeData::Document => {
            for child in node.children.borrow().iter() {
                render_node(child, output);
            }
        }
        NodeData::Doctype { .. }
        | NodeData::Comment { .. }
        | NodeData::ProcessingInstruction { .. } => {}
    }
}

fn render_element(element: &Handle, name: &str, output: &mut String) {
    if matches!(name, "style" | "script" | "head" | "svg" | "noscript") {
        return;
    }
    if name == "table" {
        render_table(element, output);
        return;
    }
    if name == "br" {
        push_line_break(output);
        return;
    }
    let block = is_block_element(name);
    if block {
        push_line_break(output);
    }
    for child in element.children.borrow().iter() {
        render_node(child, output);
    }
    if block {
        push_line_break(output);
    }
}

fn render_table(table: &Handle, output: &mut String) {
    let mut rows = Vec::new();
    collect_table_rows(table, &mut rows);
    push_line_break(output);
    for row in rows {
        let cells = row
            .children
            .borrow()
            .iter()
            .filter(|cell| matches!(element_name(cell), Some("td" | "th")))
            .map(render_table_cell)
            .collect::<Vec<_>>();
        if !cells.is_empty() {
            if cells.iter().all(String::is_empty) {
                output.push_str(&vec!["|"; cells.len()].join(" "));
            } else {
                output.push_str(&cells.join(" | "));
            }
            push_line_break(output);
        }
    }
}

fn collect_table_rows(element: &Handle, rows: &mut Vec<Handle>) {
    for child in element.children.borrow().iter() {
        match element_name(child) {
            Some("tr") => rows.push(child.clone()),
            Some("table") => {}
            _ => collect_table_rows(child, rows),
        }
    }
}

fn element_name(node: &Handle) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.as_ref()),
        _ => None,
    }
}

fn render_table_cell(cell: &Handle) -> String {
    let mut rendered = String::new();
    for child in cell.children.borrow().iter() {
        render_node(child, &mut rendered);
    }
    rendered.split_whitespace().collect::<Vec<_>>().join(" ")
}
```

Add helpers `is_block_element`, `push_line_break`, `normalize_rendered_text`, and
`sanitize_controls` matching the design. `sanitize_controls` maps every control
except `\n` and `\t` to `\u{FFFD}`. `normalize_rendered_text` collapses
intra-line whitespace, removes empty lines, and joins the rest with `\n`.

- [ ] **Step 5: Wire the private module into the public convenience API**

At the top of `lib.rs`, add:

```rust
mod markup;
```

Replace `normalize_ocr_pages` with:

```rust
pub fn normalize_ocr_pages(page_texts: &[String]) -> String {
    markup::normalize_pages(page_texts).text
}
```

- [ ] **Step 6: Run the OCR suite and make it GREEN**

Run:

```bash
cargo test -p archivist-ocr --locked
```

Expected: all old and new tests pass. If exact whitespace differs, fix the
renderer; do not weaken data-loss or column-preservation assertions.

- [ ] **Step 7: Run dependency policy checks**

```bash
cargo audit
cargo deny check
```

Expected: no advisories, denied licenses, denied sources, or yanked packages.

- [ ] **Step 8: Commit parser implementation**

```bash
git add Cargo.toml Cargo.lock crates/archivist-ocr/Cargo.toml \
  crates/archivist-ocr/src/lib.rs crates/archivist-ocr/src/markup.rs
git commit -m "feat(ocr): normalize layout markup with an HTML parser"
```

---

### Task 3: Add the dedicated layout-only validation error and worker integration

**Files:**
- Modify: `crates/archivist-ocr/src/lib.rs:259-320`
- Modify: `crates/archivist-worker/src/main.rs:40-50,1978-1980,2026-2027`

**Interfaces:**
- Consumes: `markup::normalize_pages` from Task 2.
- Produces: `normalize_and_validate_ocr_pages(&[String], usize) -> anyhow::Result<String>` used by the worker.

- [ ] **Step 1: Write the failing layout-only validation tests**

```rust
#[test]
fn normalize_and_validate_reports_layout_only_documents() {
    let pages = vec![
        r#"<div data-bbox="0 0 100 100" data-label="Picture"></div>"#.to_owned(),
    ];
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
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test -p archivist-ocr normalize_and_validate_reports_layout_only_documents --locked
```

Expected: compile failure because `normalize_and_validate_ocr_pages` does not exist.

- [ ] **Step 3: Implement the validated API**

```rust
pub fn normalize_and_validate_ocr_pages(
    page_texts: &[String],
    min_chars: usize,
) -> Result<String> {
    let normalized = markup::normalize_pages(page_texts);
    if normalized.normalization_limits_exceeded {
        return Err(anyhow!(
            "OCR layout markup exceeded normalization limits"
        ));
    }
    if normalized.had_layout_markup && normalized.text.trim().is_empty() {
        return Err(anyhow!(
            "OCR produced no text after layout markup normalization"
        ));
    }
    validate_ocr_text(&normalized.text, min_chars)?;
    Ok(normalized.text)
}
```

- [ ] **Step 4: Switch the worker to the validated API**

Replace the imports of `normalize_ocr_pages` and `validate_ocr_text` with
`normalize_and_validate_ocr_pages`. Replace:

```rust
let text = normalize_ocr_pages(&texts);
validate_ocr_text(&text, settings.ocr.min_chars)?;
```

with:

```rust
let text = normalize_and_validate_ocr_pages(&texts, settings.ocr.min_chars)?;
```

- [ ] **Step 5: Correct the cache comment without changing cache behavior**

Keep `strip_code_fences(&response.text)` and replace its comment with:

```rust
// Strip fences before caching, but intentionally keep provider layout markup
// raw. Document-level normalization runs after page assembly so parser fixes
// also apply to cached pages and HTML entities are never decoded twice.
```

- [ ] **Step 6: Verify OCR and worker tests GREEN**

```bash
cargo test -p archivist-ocr --locked
cargo test -p archivist-worker --locked
```

Expected: both packages pass with zero failures.

- [ ] **Step 7: Commit validation integration**

```bash
git add crates/archivist-ocr/src/lib.rs crates/archivist-worker/src/main.rs
git commit -m "fix(worker): distinguish layout-only OCR output"
```

---

### Task 4: Document release-boundary behavior and operational gate

**Files:**
- Modify: `docs/RELEASE_NOTES.md:8`
- Modify: `docs/USER_GUIDE.md:183-225`

**Interfaces:**
- Consumes: final normalization and error behavior from Tasks 2-3.
- Produces: operator-facing upgrade note and MinerU activation prerequisite.

- [ ] **Step 1: Add the Unreleased release-note entry**

Add under `## Unreleased`:

```markdown
- **Safe MinerU layout normalization (#322–#338):** OCR layout HTML is parsed
  with a browser-grade parser and rendered into stable archival text. Raw `<`
  comparisons, quoted attributes, entities, empty table cells, block
  boundaries, and fences exposed after parsing are covered by regressions;
  style/script/head/svg/noscript content is discarded. Layout-only output now
  fails with a dedicated permanent error instead of the generic minimum-length
  error. Enabling MinerU remains opt-in.
- **Upgrade note:** normalized content hashes change for markup-bearing OCR
  pages. Duplicate detection does not bridge old-to-new hashes across this one
  deployment boundary; no backfill is performed. New-to-new matching remains
  stable.
```

- [ ] **Step 2: Add the MinerU activation warning to the user guide**

In `### MinerU (Vision-Only OCR)`, state that production activation requires a
release containing the safe layout normalizer and that layout-only scans are
reported with the dedicated error. Do not add deployment URLs or private
topology.

- [ ] **Step 3: Verify documentation formatting and links**

```bash
node scripts/verify/docs_link_check.mjs
git diff --check
```

Expected: both commands exit 0.

- [ ] **Step 4: Commit documentation**

```bash
git add docs/RELEASE_NOTES.md docs/USER_GUIDE.md
git commit -m "docs(ocr): document MinerU normalization transition"
```

---

### Task 5: Run full gates, publish the branch, and close the OCR milestone

**Files:**
- Verify only unless a gate exposes a defect.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: reviewed merge request, merged `main`, closed #323/#324/#325/#326/#331/#335/#322, and closed milestone 50.

- [ ] **Step 1: Run formatting and static analysis**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

- [ ] **Step 2: Run build and tests**

```bash
cargo build --workspace --locked
cargo test --workspace --locked
```

- [ ] **Step 3: Run dependency and repository gates**

```bash
cargo audit
cargo deny check
node scripts/verify/docs_link_check.mjs
git diff --check
git status --short
```

- [ ] **Step 4: Review the branch diff against the spec**

Check every issue mapping in
`docs/superpowers/specs/2026-07-17-ocr-markup-normalization-design.md`. Confirm
the root checkout still contains its untouched user-owned OCR diff.

- [ ] **Step 5: Push and create a merge request**

```bash
git push -u origin codex/ocr-markup-hardening
glab mr create --source-branch codex/ocr-markup-hardening --target-branch main \
  --title "feat(ocr): safely normalize MinerU layout markup" \
  --description "Closes #323, #324, #325, #326, #331, and #335. Completes #322 after merge."
```

- [ ] **Step 6: Verify the merge-request pipeline and merge**

Inspect every job, repair failures on the same branch, and merge only when all
required jobs are green. Then verify the resulting `main` pipeline.

- [ ] **Step 7: Close issues and milestone with evidence**

Add the merge commit, test gates, and pipeline URLs to #323/#324/#325/#326/#331/#335.
Close those issues, then close #322 and milestone 50. Leave #61 and #311 open.
