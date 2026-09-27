import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import { I18nProvider, useI18n } from './I18nProvider';

const sources = import.meta.glob<string>(
  [
    '../dashboard/Dashboard.tsx',
    '../chat/DocumentChat.tsx',
    '../prompts/Prompts.tsx',
    '../audit/Audit.tsx',
    '../reviews/Reviews.tsx',
    '../debug/DebugConsole.tsx',
    '../statistics/Statistics.tsx',
    '../settings/sections/ProviderCard.tsx'
  ],
  { query: '?raw', import: 'default', eager: true }
);
const source = (rel: string) => {
  const text = sources[`../${rel}`];
  if (text === undefined) throw new Error(`source ${rel} not loaded`);
  return text;
};

/** Argument list of every `run(` call in `code` (balanced-paren scan). */
function runCallArguments(raw: string): string[] {
  const code = raw.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');
  const calls: string[] = [];
  const pattern = /(?<![\w.])run\(/g;
  let match: RegExpExecArray | null;
  while ((match = pattern.exec(code))) {
    let depth = 1;
    let index = match.index + match[0].length;
    const start = index;
    while (depth > 0 && index < code.length) {
      const char = code[index];
      if (char === '(') depth += 1;
      else if (char === ')') depth -= 1;
      index += 1;
    }
    calls.push(code.slice(start, index - 1));
  }
  return calls;
}

describe('localisation gaps stay closed (#434)', () => {
  const pages = ['dashboard/Dashboard.tsx', 'chat/DocumentChat.tsx', 'prompts/Prompts.tsx', 'audit/Audit.tsx', 'reviews/Reviews.tsx'];

  it.each(pages)('%s passes t to every run() so errors are translated', (rel) => {
    const calls = runCallArguments(source(rel));
    expect(calls.length).toBeGreaterThan(0);
    for (const args of calls) {
      expect(args.trimEnd(), `run(${args.slice(0, 60)}…)`).toMatch(/,\s*t$/);
    }
  });

  it.each(['prompts/Prompts.tsx', 'reviews/Reviews.tsx', 'debug/DebugConsole.tsx'])(
    '%s formats percentages and durations through the locale',
    (rel) => {
      expect(source(rel)).not.toMatch(/toFixed\(/);
    }
  );

  it('statistics axes use the app locale, not the browser default', () => {
    expect(source('statistics/Statistics.tsx')).not.toMatch(/DateTimeFormat\(undefined/);
  });

  it('provider kind options are translated', () => {
    const card = source('settings/sections/ProviderCard.tsx');
    expect(card).not.toMatch(/<option value="[a-z_]+">[a-z]/);
    expect(card).toContain('settings.provider.kind.${kind}');
  });
});

function Probe() {
  const { formatPercent, formatNumber, t, messageLocale } = useI18n();
  return (
    <ul>
      <li>{messageLocale}</li>
      <li data-testid="percent">{formatPercent(0.125, 1)}</li>
      <li data-testid="percent0">{formatPercent(0.4)}</li>
      <li data-testid="seconds">
        {formatNumber(1.5, { style: 'unit', unit: 'second', unitDisplay: 'narrow', minimumFractionDigits: 2 })}
      </li>
      <li data-testid="kind">{t('settings.provider.kind.openai_compatible')}</li>
    </ul>
  );
}

describe('locale-aware number helpers (#434)', () => {
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
  });

  it('formats percentages with the requested precision in English', () => {
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>
    );
    expect(screen.getByTestId('percent')).toHaveTextContent('12.5%');
    expect(screen.getByTestId('percent0')).toHaveTextContent('40%');
    expect(screen.getByTestId('seconds')).toHaveTextContent('1.50s');
    expect(screen.getByTestId('kind')).toHaveTextContent('OpenAI-compatible');
  });

  it('uses the selected app locale (German)', async () => {
    window.localStorage.setItem('paperless-archivist.ui-locale', 'de');
    render(
      <I18nProvider>
        <Probe />
      </I18nProvider>
    );
    expect(await screen.findByText('OpenAI-kompatibel')).toBeInTheDocument();
    expect(screen.getByTestId('percent').textContent).toMatch(/^12,5\s%$/);
    expect(screen.getByTestId('seconds').textContent).toMatch(/^1,50\s?\S+$/);
  });
});
