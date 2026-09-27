import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { I18nProvider } from '../i18n/I18nProvider';
import type { Prompt, PromptTestResponse } from '../api/client';

expect.extend(toHaveNoViolations);

// #446: provider/model selection in the test runner and side-by-side runs.

const prompts: Prompt[] = [
  {
    id: 'metadata-v2',
    stage: 'metadata',
    name: 'metadata-default',
    version: 2,
    content: 'Metadata prompt v2',
    active: true,
    created_at: '2026-07-18T00:00:00Z'
  },
  {
    id: 'metadata-v1',
    stage: 'metadata',
    name: 'metadata-default',
    version: 1,
    content: 'Metadata prompt v1',
    active: false,
    created_at: '2026-07-17T00:00:00Z'
  }
];

function response(title: string, model: string): PromptTestResponse {
  return {
    provider: 'cloud',
    model,
    stage: 'metadata',
    raw_text: '{}',
    parsed: {
      suggestion: { title: { title, confidence: 0.9 } },
      diagnostics: {
        status: 'valid',
        decoded_fields: ['title'],
        null_fields: [],
        invalid_fields: [],
        unknown_field_count: 0
      }
    },
    validation_errors: [],
    warnings: [],
    duration_ms: 100
  };
}

const testPrompt = vi.fn(async (input: { content: string; model?: string }) =>
  response(input.content.endsWith('v1') ? 'Old title' : 'New title', input.model ?? 'default')
);

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      prompts: vi.fn(async () => ({ items: prompts.map((prompt) => ({ ...prompt })) })),
      promptUsage: vi.fn(async () => ({ items: [] })),
      promptExperiments: vi.fn(async () => ({ items: [] })),
      settings: vi.fn(async () => ({
        ai: {
          providers: [
            { name: 'ollama', kind: 'ollama', enabled: true, default_text_model: 'qwen3:8b' },
            { name: 'cloud', kind: 'openai', enabled: true, default_text_model: 'gpt-large' },
            { name: 'off', kind: 'openai', enabled: false, default_text_model: 'x' },
            { name: 'mineru', kind: 'mineru', enabled: true, default_text_model: null }
          ]
        }
      })),
      testPrompt
    }
  };
});

async function renderMetadataPrompts() {
  const { Prompts } = await import('./Prompts');
  const view = render(
    <I18nProvider>
      <Prompts setError={() => undefined} />
    </I18nProvider>
  );
  fireEvent.click(
    within(await screen.findByRole('complementary', { name: 'Prompt stages' })).getByRole('button', {
      name: /^Metadata/
    })
  );
  await waitFor(() =>
    expect(screen.getByRole('textbox', { name: 'Prompt content' })).toHaveValue('Metadata prompt v2')
  );
  return view;
}

describe('<Prompts> provider selection and version comparison', () => {
  beforeEach(() => {
    cleanup();
    testPrompt.mockClear();
    window.localStorage.clear();
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
  });

  it('offers enabled text providers and sends the chosen provider and model', async () => {
    await renderMetadataPrompts();
    const provider = screen.getByRole('combobox', { name: 'Provider' }) as HTMLSelectElement;
    await waitFor(() => expect(within(provider).getByRole('option', { name: 'cloud' })).toBeInTheDocument());
    expect(within(provider).queryByRole('option', { name: 'off' })).toBeNull();
    expect(within(provider).queryByRole('option', { name: 'mineru' })).toBeNull();
    fireEvent.change(provider, { target: { value: 'cloud' } });
    const model = screen.getByRole('textbox', { name: 'Model' });
    expect(model).toHaveAttribute('placeholder', 'gpt-large');
    fireEvent.change(model, { target: { value: 'gpt-x' } });
    fireEvent.change(screen.getByRole('textbox', { name: 'Test sample text' }), { target: { value: 'Invoice' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test Current Editor' }));
    await waitFor(() =>
      expect(testPrompt).toHaveBeenCalledWith({
        stage: 'metadata',
        content: 'Metadata prompt v2',
        sample_text: 'Invoice',
        paperless_document_id: null,
        provider_name: 'cloud',
        model: 'gpt-x'
      })
    );
  });

  it('runs two versions against the same input and shows a field diff', async () => {
    const { container } = await renderMetadataPrompts();
    const compare = screen.getByRole('combobox', { name: 'Compare against' }) as HTMLSelectElement;
    await waitFor(() => expect(compare.value).toBe('metadata-v1'));
    fireEvent.change(screen.getByRole('textbox', { name: 'Test document ID' }), { target: { value: '42' } });
    fireEvent.change(screen.getByRole('textbox', { name: 'Model' }), { target: { value: 'qwen3:14b' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test both versions' }));

    const results = await screen.findByRole('region', { name: 'Side-by-side results' });
    expect(testPrompt).toHaveBeenCalledTimes(2);
    const [first, second] = testPrompt.mock.calls.map(([input]) => input);
    expect(first).toMatchObject({ content: 'Metadata prompt v1', paperless_document_id: 42, model: 'qwen3:14b' });
    expect(second).toMatchObject({ content: 'Metadata prompt v2', paperless_document_id: 42, model: 'qwen3:14b' });
    const table = within(results).getByRole('table', { name: '1 field(s) differ' });
    const row = within(table).getByRole('row', { name: /suggestion\.title\.title/ });
    expect(row).toHaveTextContent('"Old title"');
    expect(row).toHaveTextContent('"New title"');
    const axeResults = await axe(container, {
      rules: { region: { enabled: false }, 'color-contrast': { enabled: false } }
    });
    expect(axeResults).toHaveNoViolations();
  });

  it('reports identical outputs and a failed side without a diff table', async () => {
    testPrompt.mockImplementationOnce(async () => response('Same', 'm')).mockImplementationOnce(async () => response('Same', 'm'));
    await renderMetadataPrompts();
    fireEvent.click(await screen.findByRole('button', { name: 'Test both versions' }));
    expect(await screen.findByText('Both versions produced the same parsed output.')).toBeInTheDocument();

    testPrompt.mockImplementationOnce(async () => {
      throw new Error('provider down');
    });
    fireEvent.click(screen.getByRole('button', { name: 'Test both versions' }));
    expect(await screen.findByText(/Test failed:/)).toBeInTheDocument();
    expect(screen.queryByRole('table', { name: /differ/ })).toBeNull();
  });
});
