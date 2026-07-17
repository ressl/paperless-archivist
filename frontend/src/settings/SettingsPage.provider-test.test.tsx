import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import {
  api,
  type ProviderTestRequest,
  type ProviderTuning,
  type RuntimeSettings
} from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

const savedSecretId = '11111111-1111-4111-8111-111111111111';
const initialTuning: ProviderTuning = {
  worker_concurrency: 4,
  consensus_secondary_text_model: null,
  consensus_date_tolerance_days: null,
  text_num_ctx: null,
  vision_num_ctx: null,
  reasoning_effort: 'high',
  max_output_tokens: 1024,
  structured_output: 'auto',
  ocr_page_limit: null,
  hourly_document_limit: null,
  daily_document_limit: null,
  metadata_confidence_threshold: null,
  title_confidence_threshold: null,
  correspondent_confidence_threshold: null,
  document_type_confidence_threshold: null,
  document_date_confidence_threshold: null,
  tags_confidence_threshold: null,
  fields_confidence_threshold: null,
  max_tags: null,
  allowed_list_max: null,
  request_timeout_seconds: 90
};

function settingsFixture(): RuntimeSettings {
  return {
    paperless: {
      base_url: 'http://paperless:8000',
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
      default_provider: 'draft-provider',
      ollama_base_url: 'http://ollama:11434',
      default_text_model: 'saved-model',
      default_vision_model: 'saved-vision',
      stage_models: [],
      providers: [
        {
          name: 'draft-provider',
          kind: 'openai',
          base_url: 'https://saved.example.test/v1',
          default_text_model: 'saved-model',
          default_vision_model: 'saved-vision',
          cost_per_1m_input_tokens_usd: null,
          cost_per_1m_output_tokens_usd: null,
          secret_id: savedSecretId,
          enabled: true,
          tuning: { ...initialTuning }
        }
      ],
      external_provider_warning_acknowledged: true,
      requeue_vision_crashes_on_startup: true,
      ollama_vision_num_ctx: 8192,
      ollama_text_num_ctx: 32768,
      consensus_date_tolerance_days: 1,
      model_catalog: [
        {
          provider_kind: 'openai_compatible',
          capability: 'text',
          model_id: 'draft-model',
          label: 'Draft model',
          recommended: true,
          usage_tier: 'medium',
          context: 'test',
          modality: 'text',
          best_for: 'provider draft tests'
        }
      ]
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
      saveSettings: vi.fn(async (settings: RuntimeSettings) => settings),
      testProvider: vi.fn(async (input: ProviderTestRequest) => ({
        ok: true,
        provider: input.name,
        model: input.model
      }))
    }
  };
});

const testProviderMock = vi.mocked(api.testProvider);
const saveSettingsMock = vi.mocked(api.saveSettings);

describe('<SettingsPage> provider draft test', () => {
  beforeEach(() => {
    cleanup();
    window.localStorage.clear();
    testProviderMock.mockClear();
    saveSettingsMock.mockClear();
  });

  it('sends the visible unsaved provider, model, tuning, and transient secret', async () => {
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    let providerCard = await screen.findByRole('group', { name: 'draft-provider' });
    fireEvent.change(within(providerCard).getByRole('combobox', { name: 'Kind' }), {
      target: { value: 'openai_compatible' }
    });
    fireEvent.change(within(providerCard).getByRole('textbox', { name: 'Base URL' }), {
      target: { value: 'https://draft.example.test/v1' }
    });
    fireEvent.change(within(providerCard).getByRole('combobox', { name: 'draft-provider text model' }), {
      target: { value: 'draft-model' }
    });
    fireEvent.change(within(providerCard).getByLabelText('API key'), {
      target: { value: 'transient-draft-secret' }
    });
    fireEvent.click(within(providerCard).getByText('Tuning'));
    const maxOutput = within(providerCard).getByRole('spinbutton', {
      name: /^Max output tokens \(max_tokens\)/
    });
    fireEvent.change(maxOutput, { target: { value: '777' } });
    fireEvent.blur(maxOutput);
    fireEvent.change(within(providerCard).getByRole('textbox', { name: 'Name' }), {
      target: { value: 'renamed-provider' }
    });
    providerCard = await screen.findByRole('group', { name: 'renamed-provider' });

    const aiDefaults = screen.getByRole('group', { name: 'AI Defaults' });
    fireEvent.click(within(aiDefaults).getByRole('button', { name: 'Test' }));

    await waitFor(() =>
      expect(testProviderMock).toHaveBeenCalledWith({
        name: 'renamed-provider',
        kind: 'openai_compatible',
        base_url: 'https://draft.example.test/v1',
        model: 'draft-model',
        tuning: { ...initialTuning, max_output_tokens: 777 },
        secret_id: savedSecretId,
        secret: 'transient-draft-secret'
      })
    );
    expect(await screen.findByText(/renamed-provider.*draft-model/i)).toBeInTheDocument();
  });

  it('blocks saving and identifies conflicting providers and invalid active URLs', async () => {
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    await screen.findByRole('group', { name: 'draft-provider' });
    fireEvent.click(screen.getByRole('button', { name: 'Add Provider' }));

    let addedProvider = screen.getByRole('group', { name: /^provider-\d+$/ });
    fireEvent.change(within(addedProvider).getByRole('textbox', { name: 'Name' }), {
      target: { value: ' DRAFT-PROVIDER ' }
    });
    addedProvider = screen.getByRole('group', { name: 'DRAFT-PROVIDER' });
    fireEvent.change(within(addedProvider).getByRole('textbox', { name: 'Base URL' }), {
      target: { value: 'not-a-url' }
    });

    expect(screen.getAllByText(/conflicts with provider/i)).toHaveLength(2);
    expect(within(addedProvider).getByText(/valid HTTP\(S\) base URL/i)).toBeInTheDocument();
    const save = screen.getByRole('button', { name: 'Save' });
    expect(save).toBeDisabled();
    expect(screen.getByText(/fix the provider configuration errors before saving/i)).toBeInTheDocument();

    fireEvent.click(save);
    expect(saveSettingsMock).not.toHaveBeenCalled();
  });
});
