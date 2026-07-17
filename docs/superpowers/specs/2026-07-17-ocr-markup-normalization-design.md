# OCR Layout-Markup Normalization Design

**Date:** 2026-07-17
**Issues:** #322, #323, #324, #325, #326, #331, #335
**Status:** Approved for implementation

## Goal

Normalize HTML-like layout fragments returned by MinerU and compatible OCR
providers into stable archival text without corrupting plain text, losing table
columns, emitting database-hostile control characters, or retaining executable
and styling content.

This is a release gate for enabling MinerU in production. It is not required by
the SGLang/MiniMax text-only path.

## Context

MinerU returns Markdown, but table and layout sections may contain HTML. The
current `origin/main` only strips leaked Markdown code fences. A local
uncommitted prototype added a hand-written tag and entity scanner; the review in
#322 found silent data-loss and structural-corruption cases in that approach.

The prototype is reference material only. The production implementation starts
from tests on the clean `origin/main` implementation.

## Chosen Approach

Use Servo `html5ever` 0.39 with the matching `markup5ever_rcdom` tree sink. Tag
boundaries, comments, quoted attributes, malformed HTML, case folding, and HTML
entities therefore use a browser-grade parser rather than a project-local
scanner. The higher-level `scraper` facade was evaluated and rejected because
its CSS-selector dependency graph introduces MPL-2.0 packages that the
repository license policy intentionally does not allow.

The OCR crate owns a small deterministic DOM-to-archive-text renderer:

- recognized block elements create line boundaries;
- `<table>` is rendered row-by-row with ` | ` between every cell, including
  empty cells;
- `<style>`, `<script>`, `<head>`, `<svg>`, and `<noscript>` subtrees are
  discarded;
- inline text is preserved and whitespace is normalized only after traversal;
- control characters other than newline and tab are replaced with U+FFFD;
- leaked standalone Markdown fences are stripped after HTML rendering.

We intentionally do not use a terminal-oriented HTML-to-text renderer. Such a
renderer owns wrapping, bullets, link decoration, and table borders, which would
make archival output dependent on display width and library presentation rules.

## Markup Detection

Every page first passes through the HTML5 tokenizer for structural inspection,
but tokenization alone does not authorize rewriting. Plain text remains
byte-for-byte unchanged apart from the existing code-fence behavior unless the
token stream contains credible OCR layout markup. Only accepted, bounded layout
input is then materialized as a DOM.

A page is treated as layout markup when at least one condition holds:

1. a start-tag token has `data-bbox` or `data-label`;
2. a table start-tag token is present;
3. the trimmed page begins with a recognized layout element and the tokenizer
   observes its matched explicit closing tag;
4. at least two recognized layout elements with matched structural closing
   tags are present.

Recognized layout elements are `p`, `div`, `section`, `article`, `header`,
`footer`, `table`, `thead`, `tbody`, `tfoot`, `tr`, `td`, `th`, `ul`, `ol`,
`li`, `blockquote`, and `h1` through `h6`.

This keeps `A < B`, `<brutto>`, and prose mentioning a lone literal `<p>` on the
plain-text path while recognizing uppercase and marker-free layout documents.
End tags embedded in attributes or comments, prefix names such as
`</paragraph>`, unmatched leading end tags, and one closing token shared by
multiple start tags do not count as structural evidence. This keeps detection
linear in the source size. The detector consumes the code-fence-unwrapped view
so a whole-page fenced HTML fragment can still be recognized. Fence stripping
is repeated after rendering, because entity decoding or tag removal can reveal
a new standalone fence.

Before DOM construction, the tokenizer rejects layout input above 100,000
tokens or 256 open elements. This bounds memory and recursive rendering depth
for malformed provider responses. The checked worker API returns the permanent,
operator-distinguishable error `OCR layout markup exceeded normalization
limits`; the legacy convenience normalizer preserves the original input on that
path instead of silently losing text.

## Public Interface and Data Flow

`crates/archivist-ocr/src/markup.rs` owns private page normalization and returns
an internal summary:

```rust
pub(crate) struct NormalizedPages {
    pub text: String,
    pub had_layout_markup: bool,
    pub normalization_limits_exceeded: bool,
}

pub(crate) fn normalize_pages(page_texts: &[String]) -> NormalizedPages;
```

`crates/archivist-ocr/src/lib.rs` keeps the existing convenience API and adds a
validated API for the worker:

```rust
pub fn normalize_ocr_pages(page_texts: &[String]) -> String;

pub fn normalize_and_validate_ocr_pages(
    page_texts: &[String],
    min_chars: usize,
) -> anyhow::Result<String>;
```

The worker switches from separate normalization and validation calls to
`normalize_and_validate_ocr_pages`.

If normalized text is empty and any input page contained layout markup, the
validated API returns the permanent, operator-distinguishable error:

```text
OCR produced no text after layout markup normalization
```

Otherwise the existing minimum-length validation and error remain unchanged.
The singular page helper is not public.

## Rendering Rules

### Plain Text

Pages not classified as markup retain their original characters. HTML entities
on those pages are not decoded, because strings such as `&amp;` may be literal
printed documentation.

### Inline and Block Content

Text nodes are appended in DOM order. Block boundaries are inserted before and
after block elements, then consecutive whitespace within each output line is
collapsed. Empty lines are removed and remaining lines are joined with `\n`.

### Tables

Each `tr` is rendered independently. Direct `td` and `th` descendants become
cells; each cell is rendered recursively and normalized to a single line. Cells
are joined with ` | ` without dropping empty leading, middle, or trailing cells.
Rows are joined with `\n`. A fully empty row emits one `|` marker per empty cell
so the row and its column count remain visible. Nested tables are rendered as
content of their owning cell without being counted again as rows of the outer
table.

### Unsafe or Non-document Subtrees

`style`, `script`, `head`, `svg`, and `noscript` elements and all descendants are
ignored. Comments, doctypes, and processing instructions are ignored.

### Entities and Control Characters

The HTML parser performs named and numeric entity decoding. Unknown named
entities remain literal according to HTML parsing rules. The final sanitizer
replaces forbidden control characters with U+FFFD, preventing NUL and other
database-hostile values from entering JSONB or Paperless PATCH bodies.

## Transition Behavior

The first release changes normalized content hashes for markup-bearing pages.
Old-to-new duplicate matching across that deployment boundary is intentionally
not backfilled; release notes must document the one-time limitation. New-to-new
matching remains stable.

The page cache continues to store raw provider output after the existing fence
cleanup. Layout normalization remains late so parser fixes also apply to cached
pages and entity decoding never runs twice. The worker cache comment must state
this explicitly.

## Testing

Regression tests cover every consolidated issue:

- raw `<` comparisons inside marked-up text;
- literal `<p>` and `<brutto>` plain text;
- tag-name prefixes, end tags in attributes/comments, unmatched end tags, and
  repeated elements sharing one closer;
- uppercase and marker-free layout;
- `>` inside quoted attributes and comments;
- style/script/head/svg/noscript subtree removal;
- NUL/control numeric entities, bare ampersands, Latin-1, currency, and
  typographic entities;
- compact and pretty-printed tables, including empty cells;
- fully empty table rows;
- headings, lists, blockquotes, and implicit block boundaries;
- fences exposed by tag removal or entity decoding;
- layout-only pages producing the dedicated validation error;
- excessive nesting and repeated unclosed table cells producing the bounded
  normalization error;
- plain-text entities remaining unchanged;
- normalization idempotence for already normalized output.

Required gates:

```text
cargo test -p archivist-ocr --locked
cargo test -p archivist-worker --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --locked
cargo audit
cargo deny check
```

## Non-goals

- No generic web-page rendering or CSS support.
- No Markdown parser or Markdown reformatting.
- No PDF-direct MinerU upload.
- No cache migration or OCR-content-hash backfill.
- No decoding of entities on pages classified as plain text.
- No activation or deployment of MinerU in this change.
