import { X } from 'lucide-react';
import { AiProviderKind, ModelCatalogEntry, ModelUsageTier } from '../../api/client';
import { useI18n } from '../../i18n/I18nProvider';
import { usageTierLabel } from './helpers';

const CATALOG_PROVIDER_KINDS: AiProviderKind[] = ['ollama', 'openai', 'anthropic', 'openai_compatible'];
const CATALOG_USAGE_TIERS: ModelUsageTier[] = ['low', 'medium', 'high', 'extra_high'];

export function ModelCatalogEditor({
  catalog,
  onChange
}: {
  catalog: ModelCatalogEntry[];
  onChange: (next: ModelCatalogEntry[]) => void;
}) {
  const { t } = useI18n();
  const entries = catalog ?? [];
  const updateAt = (index: number, patch: Partial<ModelCatalogEntry>) =>
    onChange(entries.map((entry, idx) => (idx === index ? { ...entry, ...patch } : entry)));
  const removeAt = (index: number) => onChange(entries.filter((_, idx) => idx !== index));
  const add = () =>
    onChange([...entries, { provider_kind: 'ollama', capability: 'text', model_id: '', recommended: false }]);
  return (
    <details className="provider-tuning model-catalog">
      <summary>{t('settings.catalog.title')}</summary>
      <div className="provider-tuning-body">
        <p className="field-hint">{t('settings.catalog.hint')}</p>
        <table className="catalog-table">
          <thead>
            <tr>
              <th>{t('settings.catalog.col.provider')}</th>
              <th>{t('settings.catalog.col.capability')}</th>
              <th>{t('settings.catalog.col.model')}</th>
              <th>{t('settings.catalog.col.recommended')}</th>
              <th>{t('settings.catalog.col.usage')}</th>
              <th>{t('settings.catalog.col.context')}</th>
              <th>{t('settings.catalog.col.best_for')}</th>
              <th aria-label={t('settings.catalog.col.actions')} />
            </tr>
          </thead>
          <tbody>
            {entries.map((entry, index) => (
              <tr key={index}>
                <td>
                  <select
                    value={entry.provider_kind}
                    aria-label={t('settings.catalog.col.provider')}
                    onChange={(event) => updateAt(index, { provider_kind: event.target.value as AiProviderKind })}
                  >
                    {CATALOG_PROVIDER_KINDS.map((kind) => (
                      <option key={kind} value={kind}>
                        {kind}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <select
                    value={entry.capability}
                    aria-label={t('settings.catalog.col.capability')}
                    onChange={(event) => updateAt(index, { capability: event.target.value as 'text' | 'vision' })}
                  >
                    <option value="text">text</option>
                    <option value="vision">vision</option>
                  </select>
                </td>
                <td>
                  <input
                    value={entry.model_id}
                    aria-label={t('settings.catalog.col.model')}
                    onChange={(event) => updateAt(index, { model_id: event.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="checkbox"
                    checked={entry.recommended}
                    aria-label={t('settings.catalog.col.recommended')}
                    onChange={(event) => updateAt(index, { recommended: event.target.checked })}
                  />
                </td>
                <td>
                  <select
                    value={entry.usage_tier ?? ''}
                    aria-label={t('settings.catalog.col.usage')}
                    onChange={(event) =>
                      updateAt(index, {
                        usage_tier: event.target.value === '' ? null : (event.target.value as ModelUsageTier)
                      })
                    }
                  >
                    <option value="">—</option>
                    {CATALOG_USAGE_TIERS.map((tier) => (
                      <option key={tier} value={tier}>
                        {usageTierLabel(tier)}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <input
                    value={entry.context ?? ''}
                    aria-label={t('settings.catalog.col.context')}
                    onChange={(event) =>
                      updateAt(index, { context: event.target.value.trim() === '' ? null : event.target.value })
                    }
                  />
                </td>
                <td>
                  <input
                    value={entry.best_for ?? ''}
                    aria-label={t('settings.catalog.col.best_for')}
                    onChange={(event) =>
                      updateAt(index, { best_for: event.target.value.trim() === '' ? null : event.target.value })
                    }
                  />
                </td>
                <td>
                  <button
                    type="button"
                    className="icon-button"
                    aria-label={t('settings.catalog.remove')}
                    title={t('settings.catalog.remove')}
                    onClick={() => removeAt(index)}
                  >
                    <X size={16} />
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <button type="button" className="secondary" onClick={add}>
          {t('settings.catalog.add')}
        </button>
      </div>
    </details>
  );
}
