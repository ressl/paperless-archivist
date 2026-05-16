import { describe, expect, it, vi, beforeEach } from 'vitest';
import { cleanup, render, waitFor } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { I18nProvider } from '../i18n/I18nProvider';
import type { RuntimeSettings } from '../api/client';

expect.extend(toHaveNoViolations);

// Build a minimally-populated RuntimeSettings fixture. SettingsPage reads many
// nested fields at first render (workflow, tagging.tag_output_language,
// metadata, etc.) — we provide reasonable defaults so the component can render
// its shell without throwing. The shape matches the `RuntimeSettings` type
// declared in `frontend/src/api/client.ts`.
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
      providers: [],
      external_provider_warning_acknowledged: true
    },
    security: {
      audit_retention_days: 365,
      ai_artifact_retention_days: 30,
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
      enabled_stages: ['ocr', 'tags', 'title'],
      fallback_to_review_on_validation_failure: true
    },
    ocr: {
      page_limit: 25,
      min_chars: 200,
      renderer: 'pdfium',
      language_hint: null
    },
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
    fields: {
      max_fields: 10,
      confidence_threshold: 0.7,
      mappings: []
    },
    ocr_correction: {
      enabled: false,
      confidence_threshold: 0.7
    }
  } as unknown as RuntimeSettings;
}

vi.mock('../api/client', async () => {
  const actual = await vi.importActual<typeof import('../api/client')>('../api/client');
  return {
    ...actual,
    api: {
      ...actual.api,
      settings: vi.fn(async () => settingsFixture()),
      ollamaModels: vi.fn(async () => ({ provider: 'ollama', models: [] }))
    }
  };
});

describe('<SettingsPage> a11y shell', () => {
  beforeEach(() => {
    cleanup();
  });

  it('renders the settings page shell without axe violations', async () => {
    const { SettingsPage } = await import('./SettingsPage');
    const { container } = render(
      <I18nProvider>
        <SettingsPage setError={() => undefined} />
      </I18nProvider>
    );
    await waitFor(
      () => {
        expect(container.querySelector('.page')).not.toBeNull();
      },
      { timeout: 5000 }
    );
    const results = await axe(container, {
      rules: {
        region: { enabled: false },
        'color-contrast': { enabled: false }
      }
    });
    expect(results).toHaveNoViolations();
  });
});
