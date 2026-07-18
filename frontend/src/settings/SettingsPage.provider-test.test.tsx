import { beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import {
  api,
  type ProviderTestRequest,
  type ProviderTuning,
  type RuntimeSettings
} from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';

const apiState = vi.hoisted(() => ({ savedSettings: null as RuntimeSettings | null }));

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
      settings: vi.fn(async () => apiState.savedSettings ?? settingsFixture()),
      ollamaModels: vi.fn(async () => ({ provider: 'ollama', models: [] })),
      saveSettings: vi.fn(async (settings: RuntimeSettings) => {
        apiState.savedSettings = settings;
        return settings;
      }),
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
const ollamaModelsMock = vi.mocked(api.ollamaModels);

describe('<SettingsPage> provider draft test', () => {
  beforeEach(() => {
    cleanup();
    window.localStorage.clear();
    apiState.savedSettings = null;
    testProviderMock.mockClear();
    saveSettingsMock.mockClear();
    ollamaModelsMock.mockReset();
    ollamaModelsMock.mockResolvedValue({ provider: 'ollama', models: [] });
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

  it('removes an unreferenced custom provider without shifting successor drafts and persists on reload', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValueOnce(false).mockReturnValue(true);
    const { SettingsPage, shiftIndexedProviderState } = await import('./SettingsPage');
    expect(
      shiftIndexedProviderState(
        {
          2: { models: ['removed-model'] },
          3: { models: ['successor-model'] }
        },
        2
      )
    ).toEqual({ 2: { models: ['successor-model'] } });
    const firstRender = render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    await screen.findByRole('group', { name: 'draft-provider' });
    fireEvent.click(screen.getByRole('button', { name: 'Add Provider' }));
    let added = screen.getByRole('group', { name: /^provider-\d+$/ });
    const removedName = within(added).getByRole('textbox', { name: 'Name' }).getAttribute('value')!;
    fireEvent.change(within(added).getByRole('textbox', { name: 'Base URL' }), {
      target: { value: 'https://removed.example.test/v1' }
    });
    fireEvent.change(within(added).getByLabelText('API key'), {
      target: { value: 'removed-draft-secret' }
    });

    fireEvent.click(screen.getByRole('button', { name: 'Add Provider' }));
    const customProviders = screen.getAllByRole('group', { name: /^provider-\d+$/ });
    added = customProviders.find(
      (card) => within(card).getByRole('textbox', { name: 'Name' }).getAttribute('value') !== removedName
    )!;
    const successorName = within(added).getByRole('textbox', { name: 'Name' }).getAttribute('value')!;
    fireEvent.change(within(added).getByRole('textbox', { name: 'Base URL' }), {
      target: { value: 'https://successor.example.test/v1' }
    });
    fireEvent.change(within(added).getByLabelText('API key'), {
      target: { value: 'successor-draft-secret' }
    });

    const removedCard = screen.getByRole('group', { name: removedName });
    fireEvent.click(within(removedCard).getByRole('button', { name: 'Remove Provider' }));
    expect(screen.getByRole('group', { name: removedName })).toBeInTheDocument();
    expect(saveSettingsMock).not.toHaveBeenCalled();

    fireEvent.click(within(removedCard).getByRole('button', { name: 'Remove Provider' }));

    expect(confirm).toHaveBeenNthCalledWith(
      2,
      expect.stringMatching(new RegExp(`${removedName}.*Save`, 's'))
    );
    expect(screen.queryByRole('group', { name: removedName })).not.toBeInTheDocument();
    const successorCard = screen.getByRole('group', { name: successorName });
    expect(within(successorCard).getByLabelText('API key')).toHaveValue('successor-draft-secret');

    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() => expect(saveSettingsMock).toHaveBeenCalledTimes(1));
    const [saved, , submittedSecrets] = saveSettingsMock.mock.calls[0];
    const savedProviders = saved.ai?.providers ?? [];
    expect(savedProviders.map((provider) => provider.name)).not.toContain(removedName);
    expect(savedProviders.map((provider) => provider.name)).toContain(successorName);
    expect(submittedSecrets).toEqual({ [successorName]: 'successor-draft-secret' });

    firstRender.unmount();
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );
    expect(await screen.findByRole('group', { name: successorName })).toBeInTheDocument();
    expect(screen.queryByRole('group', { name: removedName })).not.toBeInTheDocument();
    confirm.mockRestore();
  });

  it('keeps built-ins disable-only and explains why referenced custom providers cannot be removed', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    const referenced = await screen.findByRole('group', { name: 'draft-provider' });
    fireEvent.click(within(referenced).getByRole('button', { name: 'Remove Provider' }));
    expect(within(referenced).getByText(/cannot be removed.*default provider/i)).toBeInTheDocument();
    expect(confirm).not.toHaveBeenCalled();

    const builtIn = screen.getByRole('group', { name: 'ollama-cloud' });
    expect(within(builtIn).queryByRole('button', { name: 'Remove Provider' })).not.toBeInTheDocument();
    expect(within(builtIn).getByText(/built-in providers cannot be removed.*disable/i)).toBeInTheDocument();
    expect(within(builtIn).getByRole('textbox', { name: 'Name' })).toBeDisabled();
    confirm.mockRestore();
  });

  it('renders the disabled SGLang MiniMax M3 preset and blocks enabling it without a URL', async () => {
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    const preset = await screen.findByRole('group', { name: 'sglang-minimax-m3' });
    expect(within(preset).getByRole('textbox', { name: 'Name' })).toBeDisabled();
    expect(within(preset).getByRole('combobox', { name: 'Kind' })).toBeDisabled();
    expect(within(preset).getByRole('combobox', { name: 'Kind' })).toHaveValue(
      'openai_compatible'
    );
    expect(within(preset).getByRole('textbox', { name: 'Base URL' })).toHaveValue('');
    expect(within(preset).getByRole('combobox', { name: 'sglang-minimax-m3 text model' })).toHaveValue(
      'ressl/MiniMax-M3-uncensored-NVFP4'
    );
    expect(
      within(preset).getByRole('combobox', { name: 'sglang-minimax-m3 vision model' })
    ).toBeEnabled();
    expect(
      within(preset).getByRole('combobox', { name: 'sglang-minimax-m3 vision model' })
    ).toHaveValue('ressl/MiniMax-M3-uncensored-NVFP4');
    expect(within(preset).getByRole('checkbox', { name: 'Enabled' })).not.toBeChecked();
    expect(within(preset).queryByRole('button', { name: 'Remove Provider' })).not.toBeInTheDocument();

    fireEvent.click(within(preset).getByRole('checkbox', { name: 'Enabled' }));

    expect(within(preset).getByText(/requires a Base URL/i)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
  });

  it('blocks removal when a stage still references the custom provider', async () => {
    const fixture = settingsFixture();
    fixture.ai.default_provider = 'ollama-cloud';
    fixture.ai.stage_models = [
      { stage: 'metadata', provider: 'draft-provider', model: 'saved-model' }
    ];
    apiState.savedSettings = fixture;
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    const referenced = await screen.findByRole('group', { name: 'draft-provider' });
    fireEvent.click(within(referenced).getByRole('button', { name: 'Remove Provider' }));
    expect(within(referenced).getByText(/cannot be removed.*stage metadata/i)).toBeInTheDocument();
    expect(confirm).not.toHaveBeenCalled();
    confirm.mockRestore();
  });

  it('keeps a custom provider editable and removable after a reserved-name draft', async () => {
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    await screen.findByRole('group', { name: 'draft-provider' });
    fireEvent.click(screen.getByRole('button', { name: 'Add Provider' }));
    const custom = screen.getByRole('group', { name: /^provider-\d+$/ });
    const name = within(custom).getByRole('textbox', { name: 'Name' });
    fireEvent.change(name, { target: { value: ' OPENAI ' } });

    const renamedCustom = screen.getByRole('group', { name: 'OPENAI' });
    expect(within(renamedCustom).getByRole('textbox', { name: 'Name' })).toBeEnabled();
    expect(
      within(renamedCustom).getByRole('button', { name: 'Remove Provider' })
    ).toBeInTheDocument();
  });

  it('restarts an unaffected in-flight model load when another provider is removed', async () => {
    const fixture = settingsFixture();
    fixture.ai.providers[0].kind = 'ollama';
    apiState.savedSettings = fixture;
    let resolveInitialLoad!: (value: { provider: string; models: [] }) => void;
    const initialLoad = new Promise<{ provider: string; models: [] }>((resolve) => {
      resolveInitialLoad = resolve;
    });
    ollamaModelsMock.mockReturnValueOnce(initialLoad);
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    const loadingProvider = await screen.findByRole('group', { name: 'draft-provider' });
    await waitFor(() =>
      expect(
        within(loadingProvider).getAllByRole('button', {
          name: 'Reload installed Ollama models'
        })[0]
      ).toBeDisabled()
    );
    fireEvent.click(screen.getByRole('button', { name: 'Add Provider' }));
    const removable = screen.getByRole('group', { name: /^provider-\d+$/ });
    fireEvent.click(within(removable).getByRole('button', { name: 'Remove Provider' }));

    await waitFor(() => expect(ollamaModelsMock).toHaveBeenCalledTimes(2));
    await waitFor(() =>
      expect(
        within(loadingProvider).getAllByRole('button', {
          name: 'Reload installed Ollama models'
        })[0]
      ).toBeEnabled()
    );
    resolveInitialLoad({ provider: 'draft-provider', models: [] });
    confirm.mockRestore();
  });

  it('recognizes a case-renamed cloud preset without injecting an unfixable duplicate', async () => {
    const fixture = settingsFixture();
    fixture.ai.providers.push({
      ...fixture.ai.providers[0],
      name: ' OLLAMA-CLOUD ',
      kind: 'ollama',
      base_url: 'https://ollama.com',
      secret_id: null,
      enabled: false
    });
    apiState.savedSettings = fixture;
    const { SettingsPage } = await import('./SettingsPage');
    render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );

    const matchingCards = await screen.findAllByRole('group', { name: /ollama-cloud/i });
    expect(matchingCards).toHaveLength(1);
    expect(within(matchingCards[0]).getByRole('textbox', { name: 'Name' })).toBeDisabled();
    expect(
      within(matchingCards[0]).queryByRole('button', { name: 'Remove Provider' })
    ).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();
  });
});
