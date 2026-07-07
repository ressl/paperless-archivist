import { useEffect, useState, type ReactNode } from 'react';
import { Info, RotateCcw } from 'lucide-react';
import {
  api,
  AiRuntimeHints,
  ProviderTuning,
  ReasoningEffort,
  RuntimeSettings
} from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { errorToString } from '../../lib/ui';
import { isOllamaCloudProvider } from './helpers';

// ---------------------------------------------------------------------------
// Provider tuning presets (v1.6.2). Mirror `ai_provider_defaults` in the
// backend; see docs/PROVIDER_TUNING_PLAN.md. The "Reset to defaults" buttons
// in the Tuning disclosure write the values for the provider's kind.
//
// Every field is explicitly listed even when null so the reset action also
// clears any operator-supplied overrides for that sub-block.
// ---------------------------------------------------------------------------

type TuningPresetKind = 'ollama' | 'ollama_cloud' | 'openai' | 'anthropic' | 'openai_compatible' | 'mineru';

export const TUNING_PRESETS: Record<TuningPresetKind, ProviderTuning> = {
  ollama: {
    worker_concurrency: 2,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    // Mirrors the backend preset: the worker floors the effective Ollama
    // text num_ctx at 32768 anyway (#304 — a long metadata prompt exceeds
    // 16384 tokens), so a smaller pin would only misrepresent what runs.
    text_num_ctx: 32768,
    vision_num_ctx: 4096,
    max_output_tokens: null,
    structured_output: null,
    ocr_page_limit: 2,
    hourly_document_limit: 200,
    daily_document_limit: 2000,
    metadata_confidence_threshold: null,
    title_confidence_threshold: null,
    correspondent_confidence_threshold: null,
    document_type_confidence_threshold: null,
    document_date_confidence_threshold: null,
    tags_confidence_threshold: null,
    fields_confidence_threshold: null,
    max_tags: null,
    allowed_list_max: null,
    request_timeout_seconds: null
  },
  ollama_cloud: {
    worker_concurrency: 4,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    max_output_tokens: null,
    structured_output: null,
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
    request_timeout_seconds: null
  },
  openai: {
    worker_concurrency: 8,
    consensus_secondary_text_model: 'gpt-4o-mini',
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    max_output_tokens: null,
    structured_output: null,
    ocr_page_limit: 8,
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
    request_timeout_seconds: null
  },
  anthropic: {
    worker_concurrency: 4,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    max_output_tokens: null,
    structured_output: null,
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
    request_timeout_seconds: null
  },
  openai_compatible: {
    worker_concurrency: 4,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    max_output_tokens: null,
    structured_output: null,
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
    request_timeout_seconds: null
  },
  mineru: {
    worker_concurrency: null,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    max_output_tokens: null,
    structured_output: null,
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
    request_timeout_seconds: null
  }
};

// Fields owned by each sub-block. Used by the per-block "Reset to defaults"
// buttons so each reset only touches its own keys.
export const PERFORMANCE_FIELDS = [
  'worker_concurrency',
  'consensus_secondary_text_model',
  'consensus_date_tolerance_days',
  'text_num_ctx',
  'vision_num_ctx',
  'reasoning_effort'
] as const satisfies readonly (keyof ProviderTuning)[];

export const CAPS_FIELDS = [
  'ocr_page_limit',
  'hourly_document_limit',
  'daily_document_limit',
  'request_timeout_seconds'
] as const satisfies readonly (keyof ProviderTuning)[];

export const THRESHOLD_FIELDS = [
  'metadata_confidence_threshold',
  'title_confidence_threshold',
  'correspondent_confidence_threshold',
  'document_type_confidence_threshold',
  'document_date_confidence_threshold',
  'tags_confidence_threshold',
  'fields_confidence_threshold',
  'max_tags',
  'allowed_list_max'
] as const satisfies readonly (keyof ProviderTuning)[];

export function tuningPresetKindFor(
  provider: Pick<RuntimeSettings['ai']['providers'][number], 'kind' | 'name' | 'base_url'>
): TuningPresetKind {
  if (provider.kind === 'ollama' && isOllamaCloudProvider(provider)) return 'ollama_cloud';
  return provider.kind;
}

// ---------------------------------------------------------------------------
// Tuning disclosure (v1.6.2). Renders the three Tuning sub-blocks and, for
// Ollama providers, the read-only "Ollama server hints" panel. The disclosure
// is implemented with native <details> elements so the layout stays
// accessible without any extra ARIA wiring; each <summary> is keyboard
// focusable and the open/close state is preserved across re-renders.
//
// `null` vs `0` semantics: a `null`/`undefined` value in `provider.tuning.<f>`
// means "inherit the global default". The number inputs render an empty
// string for null/undefined so operators can distinguish "unset" from an
// explicit zero. Writing `0` is preserved.
// ---------------------------------------------------------------------------

export type TuningField = keyof ProviderTuning;

export function TuningDisclosure({
  provider,
  globals,
  onChangeTuning,
  onResetBlock
}: {
  provider: RuntimeSettings['ai']['providers'][number];
  globals: RuntimeSettings;
  onChangeTuning: (patch: Partial<ProviderTuning>) => void;
  onResetBlock: (fields: readonly TuningField[]) => void;
}) {
  const { t } = useI18n();
  const tuning = provider.tuning ?? {};
  const isOllama = provider.kind === 'ollama';
  return (
    <details className="provider-tuning">
      <summary>{t('settings.tuning.title')}</summary>
      <div className="provider-tuning-body">
        <TuningSection titleKey="settings.tuning.section.performance" onReset={() => onResetBlock(PERFORMANCE_FIELDS)}>
          <TuningNumberField
            field="worker_concurrency"
            value={tuning.worker_concurrency}
            defaultValue={globals.workflow.mode ? null : null /* env-only fallback */}
            defaultLabel={t('settings.tuning.default.worker_concurrency_env')}
            min={1}
            step={1}
            onChange={(value) => onChangeTuning({ worker_concurrency: value })}
          />
          <TuningTextField
            field="consensus_secondary_text_model"
            value={tuning.consensus_secondary_text_model}
            defaultLabel={t('settings.tuning.default.consensus_disabled')}
            onChange={(value) => onChangeTuning({ consensus_secondary_text_model: value })}
          />
          <TuningNumberField
            field="consensus_date_tolerance_days"
            value={tuning.consensus_date_tolerance_days}
            defaultValue={1}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ consensus_date_tolerance_days: value })}
          />
          <TuningNumberField
            field="text_num_ctx"
            value={tuning.text_num_ctx}
            defaultValue={globals.ai.ollama_text_num_ctx ?? null}
            min={1024}
            step={1024}
            onChange={(value) => onChangeTuning({ text_num_ctx: value })}
          />
          <TuningNumberField
            field="vision_num_ctx"
            value={tuning.vision_num_ctx}
            defaultValue={globals.ai.ollama_vision_num_ctx ?? null}
            min={1024}
            step={1024}
            onChange={(value) => onChangeTuning({ vision_num_ctx: value })}
          />
          <TuningSelectField
            field="reasoning_effort"
            value={tuning.reasoning_effort}
            options={REASONING_EFFORT_OPTIONS}
            defaultLabel={t('settings.tuning.default.reasoning_effort')}
            hint={provider.kind === 'anthropic' ? t('settings.tuning.hint.reasoning_anthropic') : undefined}
            onChange={(value) => onChangeTuning({ reasoning_effort: value })}
          />
        </TuningSection>
        <TuningSection titleKey="settings.tuning.section.caps" onReset={() => onResetBlock(CAPS_FIELDS)}>
          <TuningNumberField
            field="ocr_page_limit"
            value={tuning.ocr_page_limit}
            defaultValue={globals.ocr.page_limit ?? null}
            min={1}
            step={1}
            onChange={(value) => onChangeTuning({ ocr_page_limit: value })}
          />
          <TuningNumberField
            field="hourly_document_limit"
            value={tuning.hourly_document_limit}
            defaultValue={globals.workflow.hourly_document_limit ?? null}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ hourly_document_limit: value })}
          />
          <TuningNumberField
            field="daily_document_limit"
            value={tuning.daily_document_limit}
            defaultValue={globals.workflow.daily_document_limit ?? null}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ daily_document_limit: value })}
          />
          <TuningNumberField
            field="request_timeout_seconds"
            value={tuning.request_timeout_seconds}
            defaultValue={180}
            min={1}
            step={10}
            onChange={(value) => onChangeTuning({ request_timeout_seconds: value })}
          />
        </TuningSection>
        <TuningSection titleKey="settings.tuning.section.thresholds" onReset={() => onResetBlock(THRESHOLD_FIELDS)}>
          <TuningNumberField
            field="metadata_confidence_threshold"
            value={tuning.metadata_confidence_threshold}
            defaultValue={globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            integer={false}
            onChange={(value) => onChangeTuning({ metadata_confidence_threshold: value })}
          />
          <TuningNumberField
            field="title_confidence_threshold"
            value={tuning.title_confidence_threshold}
            defaultValue={globals.metadata.title_confidence_threshold ?? globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            integer={false}
            onChange={(value) => onChangeTuning({ title_confidence_threshold: value })}
          />
          <TuningNumberField
            field="correspondent_confidence_threshold"
            value={tuning.correspondent_confidence_threshold}
            defaultValue={globals.metadata.correspondent_confidence_threshold ?? globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            integer={false}
            onChange={(value) => onChangeTuning({ correspondent_confidence_threshold: value })}
          />
          <TuningNumberField
            field="document_type_confidence_threshold"
            value={tuning.document_type_confidence_threshold}
            defaultValue={globals.metadata.document_type_confidence_threshold ?? globals.metadata.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            integer={false}
            onChange={(value) => onChangeTuning({ document_type_confidence_threshold: value })}
          />
          <TuningNumberField
            field="document_date_confidence_threshold"
            value={tuning.document_date_confidence_threshold}
            defaultValue={globals.metadata.document_date_confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            integer={false}
            onChange={(value) => onChangeTuning({ document_date_confidence_threshold: value })}
          />
          <TuningNumberField
            field="tags_confidence_threshold"
            value={tuning.tags_confidence_threshold}
            defaultValue={globals.metadata.tags_confidence_threshold ?? globals.tagging.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            integer={false}
            onChange={(value) => onChangeTuning({ tags_confidence_threshold: value })}
          />
          <TuningNumberField
            field="fields_confidence_threshold"
            value={tuning.fields_confidence_threshold}
            defaultValue={globals.metadata.fields_confidence_threshold ?? globals.fields.confidence_threshold}
            min={0}
            max={1}
            step={0.05}
            integer={false}
            onChange={(value) => onChangeTuning({ fields_confidence_threshold: value })}
          />
          <TuningNumberField
            field="max_tags"
            value={tuning.max_tags}
            defaultValue={globals.tagging.max_tags}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ max_tags: value })}
          />
          <TuningNumberField
            field="allowed_list_max"
            value={tuning.allowed_list_max}
            defaultValue={globals.metadata.allowed_list_max ?? null}
            min={0}
            step={1}
            onChange={(value) => onChangeTuning({ allowed_list_max: value })}
          />
        </TuningSection>
        {isOllama && <OllamaServerHints providerName={provider.name} />}
      </div>
    </details>
  );
}

function TuningSection({
  titleKey,
  onReset,
  children
}: {
  titleKey:
    | 'settings.tuning.section.performance'
    | 'settings.tuning.section.caps'
    | 'settings.tuning.section.thresholds';
  onReset: () => void;
  children: ReactNode;
}) {
  const { t } = useI18n();
  return (
    <details className="provider-tuning-section" open>
      <summary>{t(titleKey)}</summary>
      <div className="provider-tuning-fields">{children}</div>
      <button
        type="button"
        className="provider-tuning-reset"
        title={t('settings.tuning.reset_defaults')}
        onClick={onReset}
      >
        <RotateCcw size={14} /> {t('settings.tuning.reset_defaults')}
      </button>
    </details>
  );
}

export function TuningNumberField({
  field,
  value,
  defaultValue,
  defaultLabel,
  min,
  max,
  step,
  integer = true,
  onChange
}: {
  field: TuningField;
  value: number | null | undefined;
  defaultValue: number | null | undefined;
  defaultLabel?: string;
  min?: number;
  max?: number;
  step?: number;
  integer?: boolean;
  onChange: (next: number | null) => void;
}) {
  const { t } = useI18n();
  // value === null / undefined => render empty (operator sees "inherits default").
  // value === 0 => render '0' (explicit zero is preserved).
  const text = value === null || value === undefined ? '' : String(value);
  const [raw, setRaw] = useState(text);
  useEffect(() => {
    setRaw(text);
  }, [text]);
  // Blur-commit like lib/ui's NumberField (#284): hold the raw draft while
  // typing and only parse on blur, rounded (most fields are unsigned integers
  // on the backend — Option<u32>/u16) and clamped into [min, max]. The old
  // per-keystroke commit let negatives/fractions through, and the backend
  // rejected the whole settings save with an opaque body-level 422 (#314).
  // Empty or unparsable input commits `null` = "inherit the global default".
  const commit = () => {
    const trimmed = raw.trim();
    const parsed = trimmed === '' ? NaN : Number(trimmed);
    if (!Number.isFinite(parsed)) {
      onChange(null);
      setRaw('');
      return;
    }
    let next = parsed;
    if (integer) next = Math.round(next);
    if (min != null) next = Math.max(min, next);
    if (max != null) next = Math.min(max, next);
    onChange(next);
    setRaw(String(next));
  };
  const labelKey = `settings.tuning.field.${field}` as Parameters<typeof t>[0];
  const renderedDefault =
    defaultLabel ??
    (defaultValue === null || defaultValue === undefined
      ? t('settings.tuning.default.inherit')
      : t('settings.tuning.global_default', { value: defaultValue }));
  return (
    <label className="provider-tuning-field">
      <span>{t(labelKey)}</span>
      <input
        type="number"
        min={min}
        max={max}
        step={step}
        value={raw}
        placeholder={defaultValue !== null && defaultValue !== undefined ? String(defaultValue) : ''}
        onChange={(event) => setRaw(event.target.value)}
        onBlur={commit}
      />
      <small className="field-hint">{renderedDefault}</small>
    </label>
  );
}

function TuningTextField({
  field,
  value,
  defaultLabel,
  onChange
}: {
  field: TuningField;
  value: string | null | undefined;
  defaultLabel: string;
  onChange: (next: string | null) => void;
}) {
  const { t } = useI18n();
  const display = value ?? '';
  const labelKey = `settings.tuning.field.${field}` as Parameters<typeof t>[0];
  return (
    <label className="provider-tuning-field">
      <span>{t(labelKey)}</span>
      <input
        type="text"
        value={display}
        onChange={(event) => {
          const next = event.target.value;
          onChange(next.trim() === '' ? null : next);
        }}
      />
      <small className="field-hint">{defaultLabel}</small>
    </label>
  );
}

function TuningSelectField({
  field,
  value,
  options,
  defaultLabel,
  hint,
  onChange
}: {
  field: TuningField;
  value: string | null | undefined;
  options: readonly { value: string; labelKey: Parameters<ReturnType<typeof useI18n>['t']>[0] }[];
  defaultLabel: string;
  hint?: string;
  onChange: (next: ReasoningEffort | null) => void;
}) {
  const { t } = useI18n();
  const labelKey = `settings.tuning.field.${field}` as Parameters<typeof t>[0];
  return (
    <label className="provider-tuning-field">
      <span>{t(labelKey)}</span>
      <select
        value={value ?? ''}
        onChange={(event) => {
          const next = event.target.value;
          onChange(next === '' ? null : (next as ReasoningEffort));
        }}
      >
        <option value="">{defaultLabel}</option>
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {t(option.labelKey)}
          </option>
        ))}
      </select>
      <small className="field-hint">{hint ?? defaultLabel}</small>
    </label>
  );
}

const REASONING_EFFORT_OPTIONS = [
  { value: 'off', labelKey: 'settings.tuning.reasoning.off' },
  { value: 'low', labelKey: 'settings.tuning.reasoning.low' },
  { value: 'medium', labelKey: 'settings.tuning.reasoning.medium' },
  { value: 'high', labelKey: 'settings.tuning.reasoning.high' }
] as const;

function OllamaServerHints({ providerName }: { providerName: string }) {
  const { t } = useI18n();
  const [hints, setHints] = useState<AiRuntimeHints | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    api
      .aiRuntimeHints(providerName)
      .then((data) => {
        if (!cancelled) {
          setHints(data);
          setLoading(false);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(errorToString(err));
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [providerName]);
  const reachable = Boolean(hints?.reachable);
  return (
    <details className="provider-tuning-section ollama-hints" open>
      <summary>
        <span className={`status-dot ${reachable ? 'ok' : 'down'}`} aria-hidden="true" />
        {t('settings.tuning.section.ollama_hints')}
      </summary>
      <div className="provider-tuning-fields">
        {loading && <p className="field-hint">{t('generic.loading')}</p>}
        {error && !loading && (
          <p className="field-hint error">
            {t('settings.tuning.ollama_unreachable')}: {error}
          </p>
        )}
        {!loading && hints && !reachable && (
          <p className="field-hint error">{t('settings.tuning.ollama_unreachable')}</p>
        )}
        {!loading && hints && (
          <>
            <dl className="provider-tuning-meta">
              <dt>{t('settings.tuning.field.version')}</dt>
              <dd>{hints.version ?? '-'}</dd>
              <dt>{t('settings.tuning.field.loaded_models')}</dt>
              <dd>
                {hints.loaded_models && hints.loaded_models.length > 0 ? (
                  <ul>
                    {hints.loaded_models.map((model) => (
                      <li key={model.name}>
                        <code>{model.name}</code>
                        {model.size_vram_bytes != null && <> — {formatVramBytes(model.size_vram_bytes)}</>}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <span>{t('generic.none')}</span>
                )}
              </dd>
            </dl>
            {hints.hint && (
              <div className="connection-feedback warning ollama-env-warning" role="note">
                <header>
                  <Info size={16} />
                  <strong>{t('settings.tuning.hint.ollama_env')}</strong>
                </header>
                <p>{hints.hint}</p>
                <pre className="ollama-kubectl-example">
                  <code>{t('settings.tuning.hint.kubectl_example')}</code>
                </pre>
              </div>
            )}
          </>
        )}
      </div>
    </details>
  );
}

function formatVramBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 MiB';
  const mib = bytes / (1024 * 1024);
  if (mib < 1024) {
    return `${Math.round(mib)} MiB`;
  }
  const gib = mib / 1024;
  return `${gib.toFixed(2)} GiB`;
}
