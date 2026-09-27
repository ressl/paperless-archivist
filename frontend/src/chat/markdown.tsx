import { Fragment, type ReactNode } from 'react';

/**
 * Safe Markdown subset for chat answers (#449).
 *
 * Model output is untrusted (it is steered by document text), so this is a
 * deliberately small in-house renderer instead of an HTML-producing library:
 * it parses Markdown into React elements and never uses
 * `dangerouslySetInnerHTML`, so raw HTML in an answer is shown as text.
 * Links are only rendered for absolute http(s) URLs; everything else (e.g.
 * `javascript:`) degrades to plain text. `[doc:<id>]` citations become links
 * to the Paperless document when a browser-facing Paperless URL is known.
 *
 * Supported: paragraphs (single newlines kept as line breaks), headings
 * (rendered as bold paragraphs, so answers don't disturb the page outline),
 * `-`/`*`/`+` and numbered lists, `>` quotes, fenced code blocks, horizontal
 * rules, and inline code, bold, italic, strikethrough and links.
 */

export type DocumentUrl = (documentId: number) => string | null;

type Block =
  | { kind: 'paragraph'; lines: string[] }
  | { kind: 'heading'; text: string }
  | { kind: 'code'; text: string }
  | { kind: 'list'; ordered: boolean; items: string[] }
  | { kind: 'quote'; lines: string[] }
  | { kind: 'rule' };

const FENCE = /^\s*(```|~~~)/;
const HEADING = /^\s{0,3}#{1,6}\s+(.*?)\s*#*\s*$/;
const UNORDERED_ITEM = /^\s*[-*+]\s+(.*)$/;
const ORDERED_ITEM = /^\s*\d{1,9}[.)]\s+(.*)$/;
const QUOTE = /^\s*>\s?(.*)$/;
const RULE = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;

export function parseMarkdownBlocks(text: string): Block[] {
  const lines = text.replace(/\r\n?/g, '\n').split('\n');
  const blocks: Block[] = [];
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) {
      index += 1;
      continue;
    }
    const fence = line.match(FENCE);
    if (fence) {
      const body: string[] = [];
      index += 1;
      while (index < lines.length && !lines[index].trim().startsWith(fence[1])) {
        body.push(lines[index]);
        index += 1;
      }
      index += 1; // closing fence (or end of a still-streaming answer)
      blocks.push({ kind: 'code', text: body.join('\n') });
      continue;
    }
    if (RULE.test(line)) {
      blocks.push({ kind: 'rule' });
      index += 1;
      continue;
    }
    const heading = line.match(HEADING);
    if (heading) {
      blocks.push({ kind: 'heading', text: heading[1] });
      index += 1;
      continue;
    }
    const listMatch = line.match(UNORDERED_ITEM) ?? line.match(ORDERED_ITEM);
    if (listMatch) {
      const ordered = !UNORDERED_ITEM.test(line);
      const pattern = ordered ? ORDERED_ITEM : UNORDERED_ITEM;
      const items: string[] = [];
      while (index < lines.length) {
        const item = lines[index].match(pattern);
        if (item) {
          items.push(item[1]);
        } else if (lines[index].trim() && /^\s{2,}/.test(lines[index]) && items.length > 0) {
          // Indented continuation line of the previous item.
          items[items.length - 1] += `\n${lines[index].trim()}`;
        } else {
          break;
        }
        index += 1;
      }
      blocks.push({ kind: 'list', ordered, items });
      continue;
    }
    if (QUOTE.test(line)) {
      const quoted: string[] = [];
      while (index < lines.length && QUOTE.test(lines[index])) {
        quoted.push(lines[index].match(QUOTE)?.[1] ?? '');
        index += 1;
      }
      blocks.push({ kind: 'quote', lines: quoted });
      continue;
    }
    const paragraph: string[] = [];
    while (
      index < lines.length &&
      lines[index].trim() &&
      !FENCE.test(lines[index]) &&
      !HEADING.test(lines[index]) &&
      !RULE.test(lines[index]) &&
      !UNORDERED_ITEM.test(lines[index]) &&
      !ORDERED_ITEM.test(lines[index]) &&
      !QUOTE.test(lines[index])
    ) {
      paragraph.push(lines[index]);
      index += 1;
    }
    blocks.push({ kind: 'paragraph', lines: paragraph });
  }
  return blocks;
}

/** Absolute http(s) URL or null; rejects `javascript:`, `data:`, relative paths, etc. */
export function safeHttpUrl(value: string): string | null {
  try {
    const url = new URL(value);
    return url.protocol === 'http:' || url.protocol === 'https:' ? url.href : null;
  } catch {
    return null;
  }
}

// One alternation, earliest match wins. Groups: 1-2 code span, 3 citation,
// 4-5 link, 6/7 bold, 8 strikethrough, 9/10 italic.
const INLINE =
  /(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)|\[doc:\s*(\d{1,9})\]|\[([^\]\n]+)\]\(([^()\s]+)\)|\*\*(?=\S)([^*\n]+?)\*\*|__(?=\S)([^_\n]+?)__|~~(?=\S)([^~\n]+?)~~|\*(?=\S)([^*\n]+?)\*|(?<![\p{L}\p{N}_])_(?=\S)([^_\n]+?)_(?![\p{L}\p{N}_])/gu;

const MAX_INLINE_DEPTH = 4;

function renderInline(text: string, documentUrl: DocumentUrl | undefined, depth = 0): ReactNode[] {
  if (depth > MAX_INLINE_DEPTH) return [text];
  const nodes: ReactNode[] = [];
  const pattern = new RegExp(INLINE.source, INLINE.flags);
  let last = 0;
  let key = 0;
  for (let match = pattern.exec(text); match; match = pattern.exec(text)) {
    if (match.index > last) nodes.push(text.slice(last, match.index));
    last = match.index + match[0].length;
    const k = `${depth}-${key++}`;
    const [, , code, citation, linkText, linkHref, bold1, bold2, strike, italic1, italic2] = match;
    if (code !== undefined) {
      nodes.push(<code key={k}>{code}</code>);
    } else if (citation !== undefined) {
      const id = Number(citation);
      const href = documentUrl?.(id) ?? null;
      nodes.push(
        href ? (
          <a key={k} className="chat-citation" href={href} target="_blank" rel="noopener noreferrer">
            #{id}
          </a>
        ) : (
          <span key={k} className="chat-citation">
            #{id}
          </span>
        )
      );
    } else if (linkText !== undefined) {
      const href = safeHttpUrl(linkHref);
      const label = renderInline(linkText, documentUrl, depth + 1);
      nodes.push(
        href ? (
          <a key={k} href={href} target="_blank" rel="noopener noreferrer">
            {label}
          </a>
        ) : (
          <Fragment key={k}>{label}</Fragment>
        )
      );
    } else if (bold1 !== undefined || bold2 !== undefined) {
      nodes.push(<strong key={k}>{renderInline(bold1 ?? bold2, documentUrl, depth + 1)}</strong>);
    } else if (strike !== undefined) {
      nodes.push(<del key={k}>{renderInline(strike, documentUrl, depth + 1)}</del>);
    } else {
      nodes.push(<em key={k}>{renderInline(italic1 ?? italic2, documentUrl, depth + 1)}</em>);
    }
  }
  if (last < text.length) nodes.push(text.slice(last));
  return nodes;
}

function withLineBreaks(lines: string[], documentUrl: DocumentUrl | undefined): ReactNode[] {
  return lines.flatMap((line, index) => {
    const content = <Fragment key={`l${index}`}>{renderInline(line, documentUrl)}</Fragment>;
    return index === 0 ? [content] : [<br key={`b${index}`} />, content];
  });
}

export function Markdown({ text, documentUrl }: { text: string; documentUrl?: DocumentUrl }) {
  return (
    <div className="chat-markdown">
      {parseMarkdownBlocks(text).map((block, index) => {
        switch (block.kind) {
          case 'paragraph':
            return <p key={index}>{withLineBreaks(block.lines, documentUrl)}</p>;
          case 'heading':
            return (
              <p key={index} className="chat-markdown-heading">
                <strong>{renderInline(block.text, documentUrl)}</strong>
              </p>
            );
          case 'code':
            return (
              <pre key={index}>
                <code>{block.text}</code>
              </pre>
            );
          case 'list': {
            const items = block.items.map((item, itemIndex) => (
              <li key={itemIndex}>{withLineBreaks(item.split('\n'), documentUrl)}</li>
            ));
            return block.ordered ? <ol key={index}>{items}</ol> : <ul key={index}>{items}</ul>;
          }
          case 'quote':
            return <blockquote key={index}>{withLineBreaks(block.lines, documentUrl)}</blockquote>;
          case 'rule':
            return <hr key={index} />;
        }
      })}
    </div>
  );
}
