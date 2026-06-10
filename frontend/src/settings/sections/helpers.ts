import type { TFunction } from '../../i18n/I18nProvider';
import type {
  ModelCatalogEntry,
  ModelUsageTier,
  OllamaInstalledModel,
  RuntimeSettings
} from '../../api/client';
import { isOllamaCloudProvider, modelOptionLabel, modelOptions } from '../../modelCatalog';
import type { ModelCapability, ModelProviderDescriptor } from './types';

// ---------------------------------------------------------------------------
// Pure helpers shared by the Settings sections. No React here so they can be
// imported by any section without pulling in JSX.
// ---------------------------------------------------------------------------

export function sanitizeConnectionDetail(detail: string) {
  return detail
    .replace(/Bearer\s+[A-Za-z0-9._~+/=-]+/gi, 'Bearer [redacted]')
    .replace(/Token\s+[A-Za-z0-9._~+/=-]+/gi, 'Token [redacted]')
    .replace(/sk-[A-Za-z0-9_-]{8,}/gi, 'sk-[redacted]')
    .replace(/api[_-]?key["'\s:=]+[A-Za-z0-9._~+/=-]+/gi, 'api_key=[redacted]');
}

export function optionalNumber(value: string) {
  if (value.trim() === '') return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
}

export function optionalPositiveInteger(value: string) {
  if (value.trim() === '') return null;
  const parsed = Number(value);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : null;
}


export function splitTags(value: string) {
  return value
    .split(',')
    .map((tag) => tag.trim())
    .filter(Boolean);
}

export function serializeFieldMappings(mappings: RuntimeSettings['fields']['mappings']) {
  return mappings
    .map((mapping) =>
      [
        mapping.field_name,
        mapping.enabled ? 'enabled' : 'disabled',
        mapping.aliases.join('; '),
        mapping.instructions ?? ''
      ].join(' | ')
    )
    .join('\n');
}

export function parseFieldMappings(value: string): RuntimeSettings['fields']['mappings'] {
  return value
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [fieldName, enabled = 'enabled', aliases = '', instructions = ''] = line
        .split('|')
        .map((part) => part.trim());
      return {
        field_name: fieldName,
        enabled: enabled.toLowerCase() !== 'disabled',
        aliases: aliases.split(';').map((alias) => alias.trim()).filter(Boolean),
        instructions: instructions || null
      };
    })
    .filter((mapping) => mapping.field_name);
}

export function formatModelSize(sizeBytes: number | null | undefined, t?: TFunction) {
  if (!sizeBytes || sizeBytes <= 0) return t ? t('settings.ollama.unknown_size') : 'unknown size';
  return `${(sizeBytes / 1024 ** 3).toFixed(sizeBytes >= 10 * 1024 ** 3 ? 1 : 2)} GB`;
}

export function usageTierLabel(tier: ModelUsageTier): string {
  switch (tier) {
    case 'low':
      return 'Low';
    case 'medium':
      return 'Medium';
    case 'high':
      return 'High';
    case 'extra_high':
      return 'Extra High';
  }
}

export function catalogEntryLabel(entry: ModelCatalogEntry): string {
  const parts = [entry.label || entry.model_id];
  if (entry.recommended) parts.push('★');
  if (entry.usage_tier) parts.push(usageTierLabel(entry.usage_tier));
  if (entry.context) parts.push(entry.context);
  return parts.join(' · ');
}

/// Builds the dropdown options for catalog-driven providers (everything except
/// local Ollama), merging the curated catalog with a live `/v1/models` sync:
/// catalog entries keep their recommendation label; entries absent from the
/// live list are flagged; live IDs not in the catalog are appended.
export function catalogModelOptions(
  provider: ModelProviderDescriptor,
  capability: ModelCapability,
  value: string,
  catalogEntries: ModelCatalogEntry[],
  syncedModels: OllamaInstalledModel[] | null,
  t: TFunction
): { value: string; label: string }[] {
  // The editable settings catalog is the source of truth. Fall back to the
  // static modelCatalog.ts only when the settings catalog has no entry for
  // this provider kind + capability (so the dropdown is never empty).
  const entries = catalogEntries.filter(
    (entry) => entry.provider_kind === provider.kind && entry.capability === capability
  );
  const base =
    entries.length > 0
      ? entries.map((entry) => ({ value: entry.model_id, label: catalogEntryLabel(entry) }))
      : modelOptions(provider, capability, value).map((option) => ({
          value: option.value,
          label: modelOptionLabel(option)
        }));
  if (!syncedModels) {
    return base;
  }
  const liveNames = new Set(syncedModels.map((model) => model.name));
  const baseValues = new Set(base.map((option) => option.value));
  const merged = base.map((option) => ({
    value: option.value,
    label: liveNames.has(option.value)
      ? option.label
      : `${option.label} · ⚠ ${t('settings.ollama.not_listed')}`
  }));
  for (const model of syncedModels) {
    if (!baseValues.has(model.name)) {
      merged.push({ value: model.name, label: model.name });
    }
  }
  if (value && !baseValues.has(value) && !liveNames.has(value)) {
    merged.unshift({ value, label: value });
  }
  return merged;
}

export function installedOllamaModelLabel(model: OllamaInstalledModel, t: TFunction) {
  return [
    model.name,
    model.parameter_size || t('settings.ollama.unknown_parameters'),
    model.quantization_level || t('settings.ollama.unknown_quantization'),
    formatModelSize(model.size_bytes, t)
  ].join(' · ');
}

export function installedOllamaModelOptions(
  models: OllamaInstalledModel[],
  current: string,
  loaded: boolean,
  placeholder: string,
  t: TFunction
) {
  const options = models.map((model) => ({
    value: model.name,
    label: installedOllamaModelLabel(model, t)
  }));
  const hasCurrent = models.some((model) => model.name === current);
  if (current && !loaded && !hasCurrent) {
    return [{ value: current, label: current }, ...options];
  }
  if (current && loaded && !hasCurrent) {
    return [{ value: current, label: `⚠ ${current} · ${t('settings.ollama.not_installed')}` }, ...options];
  }
  if (!current && loaded && options.length === 0) {
    return [{ value: '', label: t('settings.ollama.none_installed') }];
  }
  if (!current && !loaded) {
    return [{ value: '', label: placeholder }];
  }
  return options;
}

export function ollamaSelectPlaceholder(
  state: { error: string | null; loading: boolean } | undefined,
  t: TFunction
) {
  if (state?.error) return t('settings.ollama.unavailable');
  if (state?.loading) return t('settings.ollama.loading_select');
  return t('settings.ollama.load_select');
}

export { isOllamaCloudProvider };
