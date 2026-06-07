import { useEffect, useId, useRef, useState } from 'react';
import { Info, RefreshCw } from 'lucide-react';
import type { ModelCatalogEntry } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import hardwareRecommendations from '../../hardwareRecommendations.json';
import type { ModelCapability, ModelProviderDescriptor, OllamaModelLoadState } from './types';
import {
  catalogModelOptions,
  installedOllamaModelOptions,
  isOllamaCloudProvider,
  ollamaSelectPlaceholder
} from './helpers';

type HardwareRecommendationProfile = {
  id: string;
  label: string;
  title: string;
  items: Array<{
    label: string;
    model: string;
  }>;
};

type HardwareRecommendationData = {
  profiles: HardwareRecommendationProfile[];
};

const recommendationProfile = (hardwareRecommendations as HardwareRecommendationData).profiles[0];

export function ProviderModelSelect({
  capability,
  provider,
  value,
  catalog,
  ollamaState,
  onChange,
  onRefresh
}: {
  capability: ModelCapability;
  provider: ModelProviderDescriptor;
  value: string;
  catalog: ModelCatalogEntry[];
  ollamaState?: OllamaModelLoadState;
  onChange: (value: string) => void;
  onRefresh: () => void;
}) {
  const { t } = useI18n();
  // Local Ollama discovers *installed* models (/api/tags); every other provider
  // (Ollama Cloud, OpenAI, Anthropic, OpenAI-compatible) discovers its *catalog*
  // via /v1/models. Both flow through the same sync button + status.
  const usesInstalledModels = provider.kind === 'ollama' && !isOllamaCloudProvider(provider);
  const hasReliableList = Boolean(ollamaState?.loaded && !ollamaState.error);
  const syncedModels = hasReliableList ? ollamaState?.models ?? [] : null;
  const options = usesInstalledModels
    ? installedOllamaModelOptions(
        ollamaState?.models ?? [],
        value,
        hasReliableList,
        ollamaSelectPlaceholder(ollamaState, t),
        t
      )
    : catalogModelOptions(provider, capability, value, catalog ?? [], syncedModels, t);
  const liveNames = syncedModels?.map((model) => model.name) ?? [];
  const currentIsMissing = Boolean(value) && hasReliableList && !liveNames.some((name) => name === value);

  return (
    <div className="model-select-block">
      <div className="model-select-row">
        <select
          value={value}
          aria-label={`${provider.name} ${capability} model`}
          onChange={(event) => onChange(event.target.value)}
        >
          {options.map((option) => (
            <option key={option.value || option.label} value={option.value} disabled={!option.value}>
              {option.label}
            </option>
          ))}
        </select>
        {usesInstalledModels && <HardwareRecommendationInfo />}
        <button
          className="icon-button"
          title={t('settings.ollama.reload_models')}
          aria-label={t('settings.ollama.reload_models')}
          type="button"
          disabled={ollamaState?.loading}
          onClick={onRefresh}
        >
          <RefreshCw size={16} />
        </button>
      </div>
      <OllamaModelStatus state={ollamaState} currentIsMissing={currentIsMissing} />
    </div>
  );
}

function OllamaModelStatus({
  state,
  currentIsMissing
}: {
  state?: OllamaModelLoadState;
  currentIsMissing: boolean;
}) {
  const { t } = useI18n();
  if (state?.loading) {
    return <p className="field-hint">{t('settings.ollama.loading')}</p>;
  }
  if (state?.error) {
    return <p className="field-hint error">{state.error}</p>;
  }
  if (state?.loaded && state.models.length === 0) {
    return <p className="field-hint warning">{t('settings.ollama.no_models')}</p>;
  }
  if (currentIsMissing) {
    return <p className="field-hint warning">{t('settings.ollama.model_missing')}</p>;
  }
  return null;
}

function HardwareRecommendationInfo() {
  const [open, setOpen] = useState(false);
  const wrapperRef = useRef<HTMLSpanElement | null>(null);
  const tooltipId = useId();

  useEffect(() => {
    if (!open) return undefined;
    const closeOnOutsidePointer = (event: PointerEvent) => {
      if (wrapperRef.current && !wrapperRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false);
      }
    };
    document.addEventListener('pointerdown', closeOnOutsidePointer);
    document.addEventListener('keydown', closeOnEscape);
    return () => {
      document.removeEventListener('pointerdown', closeOnOutsidePointer);
      document.removeEventListener('keydown', closeOnEscape);
    };
  }, [open]);

  if (!recommendationProfile) return null;

  return (
    <span
      className="tooltip-shell"
      ref={wrapperRef}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        className="info-button"
        type="button"
        aria-label={`Hardware recommendation for ${recommendationProfile.label}`}
        aria-describedby={open ? tooltipId : undefined}
        aria-expanded={open}
        onFocus={() => setOpen(true)}
        onClick={(event) => {
          event.preventDefault();
          setOpen((current) => !current);
        }}
      >
        <Info size={16} />
      </button>
      {open && (
        <span className="hardware-tooltip" id={tooltipId} role="tooltip">
          <strong>{recommendationProfile.title}</strong>
          {recommendationProfile.items.map((item) => (
            <span key={item.label}>
              <b>{item.label}:</b> <code>{item.model}</code>
            </span>
          ))}
        </span>
      )}
    </span>
  );
}
