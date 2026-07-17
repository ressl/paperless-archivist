use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use html5ever::tendril::TendrilSink;
use html5ever::tokenizer::states::{Rawtext, Rcdata, ScriptData};
use html5ever::tokenizer::{
    BufferQueue, EndTag, StartTag, TagToken, Token, TokenSink, TokenSinkResult, Tokenizer,
    TokenizerOpts,
};
use html5ever::{QualName, local_name, ns, parse_document, parse_fragment};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::strip_code_fences;

const MAX_LAYOUT_TOKENS: usize = 100_000;
const MAX_LAYOUT_NESTING_DEPTH: usize = 256;

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
    let pages = page_texts
        .iter()
        .map(|page| normalize_page(page))
        .collect::<Vec<_>>();
    let had_layout_markup = pages.iter().any(|page| page.had_layout_markup);
    let normalization_limits_exceeded = pages.iter().any(|page| page.normalization_limits_exceeded);
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
    // Unwrap whole-page fences for detection, then run fence removal again
    // after rendering because tag/entity decoding can reveal a new bare fence.
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
        return NormalizedPage {
            // Preserve the legacy convenience API's lossless fallback. The
            // worker consumes the checked API and rejects this page below.
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

struct MarkupInspection {
    marker_attribute: bool,
    table: bool,
    recognized_elements: usize,
    explicitly_closed_elements: usize,
    first_start_tag: Option<String>,
    first_start_tag_matched: bool,
    limits_exceeded: bool,
}

struct OpenElement {
    name: String,
    is_first_start_tag: bool,
}

#[derive(Default)]
struct MarkupInspectionSink {
    marker_attribute: Cell<bool>,
    table: Cell<bool>,
    recognized_starts: RefCell<HashMap<String, usize>>,
    recognized_ends: RefCell<HashMap<String, usize>>,
    open_elements: RefCell<Vec<OpenElement>>,
    first_start_tag: RefCell<Option<String>>,
    first_start_tag_matched: Cell<bool>,
    token_count: Cell<usize>,
    limits_exceeded: Cell<bool>,
}

impl MarkupInspectionSink {
    fn increment_recognized(counts: &RefCell<HashMap<String, usize>>, name: &str) {
        if is_layout_element(name) {
            let mut counts = counts.borrow_mut();
            *counts.entry(name.to_owned()).or_default() += 1;
        }
    }

    fn record_start_tag(&self, tag: &html5ever::tokenizer::Tag) {
        let name = tag.name.as_ref();
        let is_first_start_tag = {
            let mut first_start_tag = self.first_start_tag.borrow_mut();
            if first_start_tag.is_none() {
                *first_start_tag = Some(name.to_owned());
                true
            } else {
                false
            }
        };
        Self::increment_recognized(&self.recognized_starts, name);
        if name == "table" {
            self.table.set(true);
        }
        if tag
            .attrs
            .iter()
            .any(|attribute| matches!(attribute.name.local.as_ref(), "data-bbox" | "data-label"))
        {
            self.marker_attribute.set(true);
        }

        // HTML ignores the self-closing flag on ordinary elements, so only
        // actual void elements may be omitted from the structural stack.
        if !is_void_element(name) {
            let mut open_elements = self.open_elements.borrow_mut();
            if open_elements.len() >= MAX_LAYOUT_NESTING_DEPTH {
                self.limits_exceeded.set(true);
            } else {
                open_elements.push(OpenElement {
                    name: name.to_owned(),
                    is_first_start_tag,
                });
            }
        }
    }

    fn record_end_tag(&self, name: &str) {
        let mut open_elements = self.open_elements.borrow_mut();
        if let Some(index) = open_elements.iter().rposition(|open| open.name == name) {
            Self::increment_recognized(&self.recognized_ends, name);
            if open_elements[index].is_first_start_tag {
                self.first_start_tag_matched.set(true);
            }
            open_elements.truncate(index);
        }
    }

    fn finish(&self) -> MarkupInspection {
        let recognized_starts = self.recognized_starts.borrow();
        let recognized_ends = self.recognized_ends.borrow();
        let recognized_elements = recognized_starts.values().sum();
        let explicitly_closed_elements = recognized_starts
            .iter()
            .map(|(name, starts)| starts.min(recognized_ends.get(name).unwrap_or(&0)))
            .sum();

        MarkupInspection {
            marker_attribute: self.marker_attribute.get(),
            table: self.table.get(),
            recognized_elements,
            explicitly_closed_elements,
            first_start_tag: self.first_start_tag.borrow().clone(),
            first_start_tag_matched: self.first_start_tag_matched.get(),
            limits_exceeded: self.limits_exceeded.get(),
        }
    }
}

impl TokenSink for MarkupInspectionSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<Self::Handle> {
        let token_count = self.token_count.get().saturating_add(1);
        self.token_count.set(token_count);
        if token_count > MAX_LAYOUT_TOKENS {
            self.limits_exceeded.set(true);
        }

        if let TagToken(tag) = token {
            match tag.kind {
                StartTag => {
                    self.record_start_tag(&tag);
                    return match tag.name.as_ref() {
                        "title" | "textarea" => TokenSinkResult::RawData(Rcdata),
                        "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
                            TokenSinkResult::RawData(Rawtext)
                        }
                        "script" => TokenSinkResult::RawData(ScriptData),
                        "plaintext" => TokenSinkResult::Plaintext,
                        _ => TokenSinkResult::Continue,
                    };
                }
                EndTag => self.record_end_tag(tag.name.as_ref()),
            }
        }
        TokenSinkResult::Continue
    }
}

fn inspect_markup(source: &str) -> MarkupInspection {
    let input = BufferQueue::default();
    input.push_back(source.into());
    let tokenizer = Tokenizer::new(MarkupInspectionSink::default(), TokenizerOpts::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.finish()
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
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
            | "ul"
            | "ol"
            | "li"
            | "blockquote"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_block_element(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "ul"
            | "ol"
            | "li"
            | "blockquote"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
    )
}

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

fn push_line_break(output: &mut String) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn normalize_rendered_text(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_controls(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch == '\n' || ch == '\t' || !ch.is_control() {
                ch
            } else {
                '\u{FFFD}'
            }
        })
        .collect()
}
