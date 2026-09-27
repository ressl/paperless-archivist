import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { useState } from 'react';
import { api, type RuntimeSettings } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';
import { UnsavedChangesProvider, useNavigationGuard } from '../lib/unsavedChanges';

expect.extend(toHaveNoViolations);

function settingsFixture(): RuntimeSettings {
  return {
    paperless: {
      base_url: 'http://localhost:8000',
      public_url: null,
      token_secret_id: null,
      timeout_seconds: 30,
      login_bridge_enabled: false,
      delta_sync_enabled: false,
      delta_sync_overlap_minutes: 5,
      active_archive: 'default',
      archive_profiles: []
    },
    ai: {
      default_provider: 'ollama',
      ollama_base_url: 'http://localhost:11434',
      default_text_model: 'llama3',
      default_vision_model: 'llava',
      stage_models: [],
      // One enabled provider matching default_provider keeps provider
      // validation green, so Save is enabled.
      providers: [
        {
          name: 'ollama',
          kind: 'ollama',
          base_url: 'http://localhost:11434',
          default_text_model: 'llama3',
          default_vision_model: 'llava',
          cost_per_1m_input_tokens_usd: null,
          cost_per_1m_output_tokens_usd: null,
          secret_id: null,
          enabled: true,
          tuning: {}
        }
      ],
      external_provider_warning_acknowledged: true
    },
    security: {
      audit_retention_days: 365,
      ai_artifact_retention_days: 30,
      runs_retention_days: 365,
      ai_artifact_storage: 'redacted',
      api_token_expiry_required: false,
      api_token_default_ttl_days: 30,
      api_token_max_ttl_days: 365
    },
    notifications: {
      enabled: false,
      webhook_url_secret_id: null,
      review_queue_threshold: 50,
      repeated_failure_threshold: 5,
      cooldown_minutes: 30
    },
    workflow: {
      mode: 'manual_review',
      paused: false,
      dry_run: false,
      hourly_document_limit: null,
      daily_document_limit: null,
      tags: {},
      rules: { include_tags: [], exclude_tags: [] },
      enabled_stages: ['ocr', 'metadata'],
      fallback_to_review_on_validation_failure: true
    },
    ocr: { page_limit: 25, min_chars: 200, renderer: 'pdfium', language_hint: null },
    tagging: {
      max_tags: 6,
      allow_new_tags: false,
      confidence_threshold: 0.7,
      old_tag_strategy: 'keep_all',
      tag_output_language: 'en'
    },
    metadata: {
      overwrite_existing_correspondent: false,
      overwrite_existing_document_type: false,
      overwrite_existing_document_date: false,
      allow_new_correspondents: false,
      allow_new_document_types: false,
      confidence_threshold: 0.7,
      document_date_confidence_threshold: 0.7
    },
    fields: { max_fields: 10, confidence_threshold: 0.7, mappings: [] },
    ocr_correction: { enabled: false, confidence_threshold: 0.7 }
  } as unknown as RuntimeSettings;
}

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      settings: vi.fn(async () => settingsFixture()),
      ollamaModels: vi.fn(async () => ({ provider: 'ollama', models: [] })),
      saveSettings: vi.fn(async (settings: RuntimeSettings) => settings)
    }
  };
});

// Minimal stand-in for App's tab switch: the same hook-in point App.tsx uses.
async function renderWithNavigation() {
  const { SettingsPage } = await import('./SettingsPage');
  function Harness() {
    const [page, setPage] = useState<'settings' | 'other'>('settings');
    const requestNavigation = useNavigationGuard();
    return (
      <>
        <button type="button" onClick={() => requestNavigation(() => setPage('other'))}>
          Go elsewhere
        </button>
        {page === 'settings' ? <SettingsPage setError={() => undefined} /> : <p>Other page</p>}
      </>
    );
  }
  render(
    <I18nProvider>
      <UnsavedChangesProvider>
        <Harness />
      </UnsavedChangesProvider>
    </I18nProvider>
  );
  const paperless = await screen.findByRole('group', { name: 'Paperless' });
  const baseUrl = within(paperless).getByRole('textbox', { name: /^Base URL/ });
  return { baseUrl };
}

function dispatchBeforeUnload() {
  const event = new Event('beforeunload', { cancelable: true });
  window.dispatchEvent(event);
  return event.defaultPrevented;
}

describe('<SettingsPage> unsaved-changes guard (#423)', () => {
  beforeEach(() => {
    cleanup();
    vi.mocked(api.saveSettings).mockClear();
  });

  it('navigates away immediately when nothing changed', async () => {
    await renderWithNavigation();
    expect(screen.getByText('All changes saved')).toBeInTheDocument();
    expect(dispatchBeforeUnload()).toBe(false);

    fireEvent.click(screen.getByRole('button', { name: 'Go elsewhere' }));
    expect(await screen.findByText('Other page')).toBeInTheDocument();
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });

  it('shows a dirty indicator in the sticky save bar and asks before leaving', async () => {
    const { baseUrl } = await renderWithNavigation();
    fireEvent.change(baseUrl, { target: { value: 'http://paperless.example.test' } });

    const saveBar = screen.getByRole('region', { name: 'Save settings' });
    expect(within(saveBar).getByText('Unsaved changes')).toBeInTheDocument();
    expect(within(saveBar).getByRole('button', { name: 'Save' })).toBeEnabled();
    expect(dispatchBeforeUnload()).toBe(true);

    fireEvent.click(screen.getByRole('button', { name: 'Go elsewhere' }));
    const dialog = await screen.findByRole('alertdialog', { name: 'Leave with unsaved changes?' });
    expect(await axe(dialog)).toHaveNoViolations();
    fireEvent.click(within(dialog).getByRole('button', { name: 'Stay on page' }));

    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument());
    expect(screen.queryByText('Other page')).not.toBeInTheDocument();
    expect(baseUrl).toHaveValue('http://paperless.example.test');

    fireEvent.click(screen.getByRole('button', { name: 'Go elsewhere' }));
    fireEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Discard and leave' })
    );
    expect(await screen.findByText('Other page')).toBeInTheDocument();
    expect(dispatchBeforeUnload()).toBe(false);
  });

  it('clears the dirty state after saving so navigation no longer asks', async () => {
    const { baseUrl } = await renderWithNavigation();
    fireEvent.change(baseUrl, { target: { value: 'http://paperless.example.test' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() => expect(api.saveSettings).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.queryByText('Unsaved changes')).not.toBeInTheDocument());

    fireEvent.click(screen.getByRole('button', { name: 'Go elsewhere' }));
    expect(await screen.findByText('Other page')).toBeInTheDocument();
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });
});
