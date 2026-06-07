import type { RuntimeSettings } from '../../api/client';
import type { TFunction } from '../../i18n/I18nProvider';
import type { ConnectionTestState, ModelProviderDescriptor } from './types';
import { sanitizeConnectionDetail } from './helpers';

// ---------------------------------------------------------------------------
// Connection-test feedback builders + problem analysers. Pure functions that
// turn a raw API result/error into a localized ConnectionTestState. Used by the
// run*Test handlers in SettingsPage.
// ---------------------------------------------------------------------------

export function paperlessTestSuccess(t: TFunction): ConnectionTestState {
  return {
    status: 'success',
    title: t('settings.paperless.success.title'),
    description: t('settings.paperless.success.description'),
    hints: [t('settings.paperless.success.hint')]
  };
}

export function paperlessTestFailure(error: string | undefined, t: TFunction): ConnectionTestState {
  const details = sanitizeConnectionDetail(error || 'Paperless test failed');
  return {
    status: 'error',
    title: t('settings.paperless.failure.title'),
    description: paperlessProblemDescription(details, t),
    hints: paperlessProblemHints(details, t),
    details
  };
}

export function paperlessUnsavedSettingsFeedback(
  settings: RuntimeSettings,
  savedSettings: RuntimeSettings,
  token: string,
  t: TFunction
): ConnectionTestState {
  const changedFields = [
    settings.paperless.base_url.trim() !== savedSettings.paperless.base_url.trim() ? 'Base URL' : null,
    settings.paperless.timeout_seconds !== savedSettings.paperless.timeout_seconds ? 'Timeout' : null,
    settings.paperless.login_bridge_enabled !== savedSettings.paperless.login_bridge_enabled
      ? 'Login bridge'
      : null,
    token.trim() ? 'API token' : null
  ].filter(Boolean);
  return {
    status: 'error',
    title: t('settings.paperless.unsaved.title'),
    description: t('settings.paperless.unsaved.description'),
    hints: [
      t('settings.paperless.unsaved.changed', { fields: changedFields.join(', ') }),
      t('settings.paperless.unsaved.save_first'),
      t('settings.paperless.unsaved.saved_url', { url: savedSettings.paperless.base_url || t('generic.empty') })
    ],
    details: `Unsaved Paperless settings. Current Base URL: ${
      settings.paperless.base_url || '(empty)'
    }; saved Base URL: ${savedSettings.paperless.base_url || '(empty)'}`
  };
}

export function paperlessBaseUrlProblem(
  baseUrl: string
): { reason: 'invalid' | 'self'; baseUrl: string; appOrigin?: string } | null {
  const trimmed = baseUrl.trim();
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return { reason: 'invalid', baseUrl: trimmed };
  }
  if (typeof window !== 'undefined' && parsed.host === window.location.host) {
    return { reason: 'self', baseUrl: trimmed, appOrigin: window.location.origin };
  }
  return null;
}

export function paperlessBaseUrlProblemFeedback(
  problem: { reason: 'invalid' | 'self'; baseUrl: string; appOrigin?: string },
  t: TFunction
): ConnectionTestState {
  if (problem.reason === 'invalid') {
    return {
      status: 'error',
      title: t('settings.paperless.invalid_url.title'),
      description: t('settings.paperless.invalid_url.description'),
      hints: [
        t('settings.paperless.hint.backend_url'),
        t('settings.paperless.hint.compose_example'),
        t('settings.paperless.hint.save_retry')
      ],
      details: `Invalid Paperless Base URL: ${problem.baseUrl || '(empty)'}`
    };
  }
  return {
    status: 'error',
    title: t('settings.paperless.self_url.title'),
    description: t('settings.paperless.self_url.description'),
    hints: [
      t('settings.paperless.hint.not_archivist'),
      t('settings.paperless.hint.kubernetes_internal'),
      t('settings.paperless.hint.save_retry')
    ],
    details: `Paperless Base URL points to Archivist itself: ${problem.baseUrl}. App origin: ${
      problem.appOrigin ?? 'unknown'
    }`
  };
}

export function providerTestSuccess(provider: ModelProviderDescriptor, t: TFunction): ConnectionTestState {
  const providerName = provider.name || provider.kind;
  const isOllama = provider.kind === 'ollama';
  return {
    status: 'success',
    title: t('settings.provider.success.title'),
    description: isOllama
      ? t('settings.provider.success.ollama', { provider: providerName })
      : t('settings.provider.success.generic', { provider: providerName }),
    hints: isOllama
      ? [t('settings.provider.success.ollama_hint')]
      : [t('settings.provider.success.generic_hint')]
  };
}

export function providerTestFailure(
  provider: ModelProviderDescriptor,
  error: string | undefined,
  t: TFunction
): ConnectionTestState {
  const details = sanitizeConnectionDetail(error || 'Provider test failed');
  return {
    status: 'error',
    title: t('settings.provider.failure.title'),
    description: providerProblemDescription(provider, details, t),
    hints: providerProblemHints(provider, details, t),
    details
  };
}

export function paperlessProblemDescription(details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (lower.includes('points to the paperless-ngx service') || lower.includes('406') || lower.includes('not acceptable')) {
    return t('settings.paperless.failure.not_acceptable');
  }
  if (lower.includes('api token') || lower.includes('secret') || lower.includes('token')) {
    return t('settings.paperless.failure.token');
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return t('settings.paperless.failure.auth');
  }
  if (lower.includes('404')) {
    return t('settings.paperless.failure.not_found');
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return t('settings.paperless.failure.timeout');
  }
  if (lower.includes('connect') || lower.includes('dns') || lower.includes('resolve') || lower.includes('refused')) {
    return t('settings.paperless.failure.network');
  }
  return t('settings.paperless.failure.default');
}

export function paperlessProblemHints(details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (lower.includes('points to the paperless-ngx service') || lower.includes('406') || lower.includes('not acceptable')) {
    return [
      t('settings.paperless.hint.real_api'),
      t('settings.paperless.hint.internal_service'),
      t('settings.paperless.hint.kubernetes_internal'),
      t('settings.paperless.hint.save_retry')
    ];
  }
  if (
    lower.includes('api token') ||
    lower.includes('secret') ||
    lower.includes('token') ||
    lower.includes('401') ||
    lower.includes('403')
  ) {
    return [
      t('settings.paperless.hint.new_token'),
      t('settings.paperless.hint.save_token'),
      t('settings.paperless.hint.permissions')
    ];
  }
  if (lower.includes('404')) {
    return [
      t('settings.paperless.hint.url_root'),
      t('settings.paperless.hint.backend_reachability'),
      t('settings.paperless.hint.compose_example')
    ];
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return [
      t('settings.paperless.hint.running'),
      t('settings.paperless.hint.network'),
      t('settings.paperless.hint.timeout')
    ];
  }
  return [
    t('settings.paperless.hint.backend_reachability'),
    t('settings.paperless.hint.network'),
    t('settings.paperless.hint.save_retry')
  ];
}

export function paperlessSettingsChanged(
  settings: RuntimeSettings,
  savedSettings: RuntimeSettings,
  token: string
) {
  return (
    settings.paperless.base_url.trim() !== savedSettings.paperless.base_url.trim() ||
    (settings.paperless.public_url ?? '').trim() !== (savedSettings.paperless.public_url ?? '').trim() ||
    settings.paperless.timeout_seconds !== savedSettings.paperless.timeout_seconds ||
    settings.paperless.login_bridge_enabled !== savedSettings.paperless.login_bridge_enabled ||
    settings.paperless.delta_sync_enabled !== savedSettings.paperless.delta_sync_enabled ||
    settings.paperless.delta_sync_overlap_minutes !== savedSettings.paperless.delta_sync_overlap_minutes ||
    settings.paperless.active_archive.trim() !== savedSettings.paperless.active_archive.trim() ||
    Boolean(token.trim())
  );
}

export function providerProblemDescription(provider: ModelProviderDescriptor, details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (provider.kind === 'ollama') {
    if (lower.includes('model') && lower.includes('not listed')) {
      return t('settings.provider.failure.ollama_missing_model');
    }
    if (lower.includes('timeout') || lower.includes('timed out')) {
      return t('settings.provider.failure.ollama_timeout');
    }
    if (lower.includes('connect') || lower.includes('dns') || lower.includes('resolve') || lower.includes('refused')) {
      return t('settings.provider.failure.ollama_network');
    }
    return t('settings.provider.failure.ollama_default');
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return t('settings.provider.failure.auth');
  }
  if (lower.includes('model')) {
    return t('settings.provider.failure.model');
  }
  if (lower.includes('timeout') || lower.includes('timed out')) {
    return t('settings.provider.failure.timeout');
  }
  return t('settings.provider.failure.default');
}

export function providerProblemHints(provider: ModelProviderDescriptor, details: string, t: TFunction) {
  const lower = details.toLowerCase();
  if (provider.kind === 'ollama') {
    if (lower.includes('model') && lower.includes('not listed')) {
      return [
        t('settings.provider.hint.install_model'),
        t('settings.provider.hint.refresh_save'),
        t('settings.provider.hint.text_vision')
      ];
    }
    return [
      t('settings.provider.hint.ollama_running'),
      t('settings.provider.hint.ollama_url'),
      t('settings.provider.hint.ollama_tags')
    ];
  }
  if (lower.includes('401') || lower.includes('403') || lower.includes('unauthorized') || lower.includes('forbidden')) {
    return [
      t('settings.provider.hint.api_key'),
      t('settings.provider.hint.model_access'),
      t('settings.provider.hint.base_url')
    ];
  }
  if (lower.includes('model')) {
    return [
      t('settings.provider.hint.supported_model'),
      t('settings.provider.hint.model_access'),
      t('settings.paperless.hint.save_retry')
    ];
  }
  return [
    t('settings.provider.hint.base_url'),
    t('settings.provider.hint.rate_limits'),
    t('settings.paperless.hint.save_retry')
  ];
}
