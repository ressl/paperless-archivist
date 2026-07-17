import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { I18nProvider } from '../i18n/I18nProvider';
import type { Prompt, PromptTestResponse } from '../api/client';

const prompts: Prompt[] = [
  {
    id: 'ocr-v1',
    stage: 'ocr',
    name: 'ocr-default',
    version: 1,
    content: 'OCR system prompt',
    active: true,
    created_at: '2026-07-16T00:00:00Z'
  },
  {
    id: 'metadata-v1',
    stage: 'metadata',
    name: 'metadata-default',
    version: 1,
    content: 'Metadata system prompt',
    active: true,
    created_at: '2026-07-17T00:00:00Z'
  }
];

let promptTestResponse: PromptTestResponse;
const testPrompt = vi.fn(async () => promptTestResponse);

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      prompts: vi.fn(async () => ({ items: prompts.map((prompt) => ({ ...prompt })) })),
      promptUsage: vi.fn(async () => ({ items: [] })),
      promptExperiments: vi.fn(async () => ({ items: [] })),
      testPrompt
    }
  };
});

async function renderMetadataPromptTester() {
  const { Prompts } = await import('./Prompts');
  render(
    <I18nProvider>
      <Prompts setError={() => undefined} />
    </I18nProvider>
  );
  const version = (await screen.findByRole('combobox', { name: 'Version' })) as HTMLSelectElement;
  await waitFor(() => expect(version.value).toBe('ocr-v1'));
  await waitFor(() =>
    expect(screen.getByRole('textbox', { name: 'Prompt content' })).toHaveValue(
      'OCR system prompt'
    )
  );
  fireEvent.click(
    within(screen.getByRole('complementary', { name: 'Prompt stages' })).getByRole('button', {
      name: /^Metadata/
    })
  );
  await waitFor(() => expect(version.value).toBe('metadata-v1'));
  await waitFor(() =>
    expect(screen.getByRole('textbox', { name: 'Prompt content' })).toHaveValue(
      'Metadata system prompt'
    )
  );
  fireEvent.change(screen.getByRole('textbox', { name: 'Test sample text' }), {
    target: { value: 'Invoice 41 from Acme AG' }
  });
  fireEvent.click(screen.getByRole('button', { name: 'Test Current Editor' }));
}

describe('<Prompts> metadata test runner', () => {
  beforeEach(() => {
    cleanup();
    testPrompt.mockClear();
    window.localStorage.clear();
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
  });

  it('renders provider, model, typed parsed output, and raw output for a valid response', async () => {
    promptTestResponse = {
      provider: 'sglang',
      model: 'ressl/MiniMax-M3-uncensored-NVFP4',
      stage: 'metadata',
      raw_text: '{"title":{"title":"Invoice 41","confidence":0.98}}',
      parsed: {
        suggestion: {
          title: { title: 'Invoice 41', confidence: 0.98 }
        },
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
      duration_ms: 1234
    };

    await renderMetadataPromptTester();

    expect(await screen.findByText('sglang / ressl/MiniMax-M3-uncensored-NVFP4')).toBeInTheDocument();
    expect(screen.getByRole('status', { name: 'Valid' })).toBeInTheDocument();
    expect(screen.getByText(/"title": "Invoice 41"/)).toBeInTheDocument();
    expect(screen.getByText(promptTestResponse.raw_text)).toBeInTheDocument();
    expect(testPrompt).toHaveBeenCalledWith({
      stage: 'metadata',
      content: 'Metadata system prompt',
      sample_text: 'Invoice 41 from Acme AG',
      paperless_document_id: null
    });
  });

  it('renders validation errors, warnings, and retained safe fields for an invalid response', async () => {
    promptTestResponse = {
      provider: 'sglang',
      model: 'ressl/MiniMax-M3-uncensored-NVFP4',
      stage: 'metadata',
      raw_text: '{"title":{"title":"Retained","confidence":0.8},"tags":"wrong"}',
      parsed: {
        suggestion: {
          title: { title: 'Retained', confidence: 0.8 }
        },
        diagnostics: {
          status: 'contract_violation',
          decoded_fields: ['title'],
          null_fields: [],
          invalid_fields: ['tags'],
          unknown_field_count: 0
        }
      },
      validation_errors: [
        'metadata field(s) have wrong types or unknown nested properties: tags'
      ],
      warnings: ['date inferred'],
      duration_ms: 420
    };

    await renderMetadataPromptTester();

    expect(await screen.findByRole('status', { name: 'Failed' })).toBeInTheDocument();
    expect(
      screen.getByText('metadata field(s) have wrong types or unknown nested properties: tags')
    ).toBeInTheDocument();
    expect(screen.getByText('date inferred')).toBeInTheDocument();
    expect(screen.getByText(/"title": "Retained"/)).toBeInTheDocument();
    expect(screen.getByText(/"status": "contract_violation"/)).toBeInTheDocument();
  });
});
