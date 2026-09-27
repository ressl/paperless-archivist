import { cleanup, render, screen } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { afterEach, describe, expect, it } from 'vitest';
import { Markdown, parseMarkdownBlocks, safeHttpUrl } from './markdown';

expect.extend(toHaveNoViolations);

const documentUrl = (id: number) => `https://paperless.example/documents/${id}/details`;

function renderMarkdown(text: string, withLinks = true) {
  return render(<Markdown text={text} documentUrl={withLinks ? documentUrl : undefined} />).container;
}

describe('chat Markdown renderer (#449)', () => {
  afterEach(cleanup);

  it('never turns raw HTML from the model into elements', () => {
    const container = renderMarkdown(
      [
        '<script>window.__pwned = true</script>',
        '<img src=x onerror="window.__pwned = true">',
        '**<iframe src="https://evil.example"></iframe>**',
        '<a href="javascript:alert(1)">click</a>'
      ].join('\n\n')
    );
    expect(container.querySelector('script, img, iframe, a[href^="javascript"]')).toBeNull();
    expect(container.querySelectorAll('[onerror]')).toHaveLength(0);
    expect(container).toHaveTextContent('<script>window.__pwned = true</script>');
    expect(container).toHaveTextContent('<img src=x onerror="window.__pwned = true">');
    expect((window as unknown as { __pwned?: boolean }).__pwned).toBeUndefined();
  });

  it('only links absolute http(s) URLs', () => {
    const container = renderMarkdown(
      '[safe](https://example.com/a) [js](javascript:alert(1)) [data](data:text/html,x) [rel](/api/settings) [vb](vbscript:x)'
    );
    const links = Array.from(container.querySelectorAll('a'));
    expect(links).toHaveLength(1);
    expect(links[0]).toHaveAttribute('href', 'https://example.com/a');
    expect(links[0]).toHaveAttribute('rel', 'noopener noreferrer');
    expect(links[0]).toHaveAttribute('target', '_blank');
    expect(container.querySelectorAll('a:not([href^="https:"])')).toHaveLength(0);
    expect(safeHttpUrl('javascript:alert(1)')).toBeNull();
    expect(safeHttpUrl('  https://x.example ')).toBe('https://x.example/');
  });

  it('renders the supported subset and links citations to Paperless', async () => {
    const container = renderMarkdown(
      [
        '## Summary',
        'Invoice **ACME** is _due_ on `2026-10-01` ~~maybe~~ [doc:12].',
        '',
        '- first',
        '- second',
        '',
        '1. one',
        '2. two',
        '',
        '> quoted',
        '',
        '```',
        '<b>code</b>',
        '```'
      ].join('\n')
    );
    expect(container.querySelector('strong')).toHaveTextContent('Summary');
    expect(screen.getByText('ACME').tagName).toBe('STRONG');
    expect(screen.getByText('due').tagName).toBe('EM');
    expect(screen.getByText('2026-10-01').tagName).toBe('CODE');
    expect(screen.getByText('maybe').tagName).toBe('DEL');
    expect(container.querySelectorAll('ul > li')).toHaveLength(2);
    expect(container.querySelectorAll('ol > li')).toHaveLength(2);
    expect(container.querySelector('blockquote')).toHaveTextContent('quoted');
    expect(container.querySelector('pre code')).toHaveTextContent('<b>code</b>');
    expect(container.querySelector('pre b')).toBeNull();
    const citation = screen.getByRole('link', { name: '#12' });
    expect(citation).toHaveAttribute('href', 'https://paperless.example/documents/12/details');
    expect(await axe(container)).toHaveNoViolations();
  });

  it('keeps citations as text when no Paperless URL is known', () => {
    const container = renderMarkdown('See [doc:7].', false);
    expect(container.querySelector('a')).toBeNull();
    expect(container).toHaveTextContent('See #7.');
  });

  it('treats an unterminated code fence (still streaming) as code', () => {
    const blocks = parseMarkdownBlocks('text\n```\nlet x = 1;');
    expect(blocks).toEqual([
      { kind: 'paragraph', lines: ['text'] },
      { kind: 'code', text: 'let x = 1;' }
    ]);
  });
});
