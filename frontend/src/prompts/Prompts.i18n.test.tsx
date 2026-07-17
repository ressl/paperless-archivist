import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { Prompt } from '../api/client';
import { I18nProvider, useI18n } from '../i18n/I18nProvider';

const prompt: Prompt = {
  id: 'ocr-v1',
  stage: 'ocr',
  name: 'default',
  version: 1,
  content: 'Read the page faithfully.',
  active: true,
  created_at: '2026-07-17T00:00:00Z'
};

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      prompts: vi.fn(async () => ({ items: [{ ...prompt }] })),
      promptUsage: vi.fn(async () => ({ items: [] })),
      promptExperiments: vi.fn(async () => ({ items: [] }))
    }
  };
});

function GermanLocaleButton() {
  const { setLocale } = useI18n();
  return (
    <button type="button" onClick={() => setLocale('de')}>
      switch-de
    </button>
  );
}

describe('<Prompts> localized stage help', () => {
  beforeEach(() => {
    cleanup();
    window.localStorage.clear();
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
  });

  it('updates mounted OCR guidance and examples when the locale changes', async () => {
    const { Prompts } = await import('./Prompts');
    render(
      <I18nProvider>
        <GermanLocaleButton />
        <Prompts setError={() => undefined} />
      </I18nProvider>
    );

    expect(
      await screen.findByText('Plain text only. No JSON, markdown fences, summaries, or commentary.')
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'switch-de' }));

    expect(
      await screen.findByText('Nur Klartext. Kein JSON, keine Markdown-Codeblöcke, Zusammenfassungen oder Kommentare.')
    ).toBeInTheDocument();
    expect(screen.getByText('Verwende [unleserlich] für nicht lesbare Textstellen.')).toBeInTheDocument();
    expect(
      screen.queryByText('Plain text only. No JSON, markdown fences, summaries, or commentary.')
    ).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /Metadaten/ }));
    expect(
      await screen.findByText(/Striktes JSON-Objekt:.*Lasse Schlüssel ohne eindeutigen Beleg weg\./)
    ).toBeInTheDocument();
    expect(
      screen.getByText('{"title":{"title":"Rechnung Acme GmbH 2026-04-12","confidence":0.92}}')
    ).toBeInTheDocument();
  });
});
