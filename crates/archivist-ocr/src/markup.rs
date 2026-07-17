use html5ever::tendril::TendrilSink;
use html5ever::{QualName, local_name, ns, parse_document, parse_fragment};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use crate::strip_code_fences;

pub(crate) struct NormalizedPages {
    pub text: String,
    pub had_layout_markup: bool,
}

struct NormalizedPage {
    text: String,
    had_layout_markup: bool,
}

pub(crate) fn normalize_pages(page_texts: &[String]) -> NormalizedPages {
    let pages = page_texts
        .iter()
        .map(|page| normalize_page(page))
        .collect::<Vec<_>>();
    let had_layout_markup = pages.iter().any(|page| page.had_layout_markup);
    let text = pages
        .into_iter()
        .map(|page| page.text.trim().to_owned())
        .filter(|page| !page.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    NormalizedPages {
        text,
        had_layout_markup,
    }
}

fn normalize_page(page: &str) -> NormalizedPage {
    // Unwrap whole-page fences for detection, then run fence removal again
    // after rendering because tag/entity decoding can reveal a new bare fence.
    let detection_input = strip_code_fences(page);
    let dom = parse_html(&detection_input);
    if !looks_like_layout_markup(&detection_input, &dom) {
        return NormalizedPage {
            text: detection_input,
            had_layout_markup: false,
        };
    }

    let mut rendered = String::with_capacity(detection_input.len());
    render_node(&dom.document, &mut rendered);
    let normalized = normalize_rendered_text(&sanitize_controls(&rendered));

    NormalizedPage {
        text: strip_code_fences(&normalized),
        had_layout_markup: true,
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

#[derive(Default)]
struct LayoutEvidence {
    marker_attribute: bool,
    table: bool,
    recognized_elements: usize,
    explicitly_closed_elements: usize,
}

fn looks_like_layout_markup(source: &str, dom: &RcDom) -> bool {
    let source_lower = source.to_ascii_lowercase();
    let mut evidence = LayoutEvidence::default();
    collect_layout_evidence(&dom.document, &source_lower, &mut evidence);

    let starts_with_layout = source_lower
        .trim_start()
        .strip_prefix('<')
        .is_some_and(|rest| {
            let name = rest
                .split(|ch: char| ch.is_ascii_whitespace() || ch == '>' || ch == '/')
                .next()
                .unwrap_or_default();
            is_layout_element(name)
        });

    evidence.marker_attribute
        || evidence.table
        || (starts_with_layout && evidence.explicitly_closed_elements >= 1)
        || (evidence.recognized_elements >= 2 && evidence.explicitly_closed_elements >= 2)
}

fn collect_layout_evidence(node: &Handle, source_lower: &str, evidence: &mut LayoutEvidence) {
    if let NodeData::Element { name, attrs, .. } = &node.data {
        let element_name = name.local.as_ref();
        if attrs
            .borrow()
            .iter()
            .any(|attribute| matches!(attribute.name.local.as_ref(), "data-bbox" | "data-label"))
        {
            evidence.marker_attribute = true;
        }
        if element_name == "table" {
            evidence.table = true;
        }
        if is_layout_element(element_name) {
            evidence.recognized_elements += 1;
            if source_lower.contains(&format!("</{element_name}")) {
                evidence.explicitly_closed_elements += 1;
            }
        }
    }

    for child in node.children.borrow().iter() {
        collect_layout_evidence(child, source_lower, evidence);
    }
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
            output.push_str(&cells.join(" | "));
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
