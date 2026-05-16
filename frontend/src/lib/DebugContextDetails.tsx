import type { InventoryItem, ReviewItem } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';

export function DebugContextDetails({
  context,
  compact
}: {
  context?: InventoryItem['debug_context'] | ReviewItem['debug_context'] | null;
  compact?: boolean;
}) {
  const { t } = useI18n();
  if (!context) return <span className="field-hint">-</span>;
  const reasonRaw = context.selector_reason ?? context.next_required_stage ?? null;
  const reason = reasonRaw && reasonRaw.trim().length > 0 ? reasonRaw : null;
  const promptLanguage = context.prompt_language ?? context.detected_language ?? 'und';
  const tagLanguage = context.tag_output_language ?? '-';
  const summary = reason
    ? compact
      ? t('inventory.debug_summary', { reason, promptLanguage, tagLanguage })
      : t('review.debug_summary', { reason, promptLanguage, tagLanguage })
    : compact
      ? t('inventory.debug_summary_no_selector', { promptLanguage, tagLanguage })
      : t('review.debug_summary_no_selector', { promptLanguage, tagLanguage });
  return (
    <details className={`debug-context${compact ? ' compact' : ''}`}>
      <summary>{summary}</summary>
      <dl>
        <div><dt>{t('inventory.language')}</dt><dd>{promptLanguage}</dd></div>
        <div><dt>{t('settings.workflow.tag_output_language')}</dt><dd>{tagLanguage}</dd></div>
        <div><dt>{t('settings.workflow.mode')}</dt><dd>{context.workflow_mode ?? '-'}</dd></div>
        <div><dt>{t('settings.workflow.paused')}</dt><dd>{context.workflow_paused ? t('generic.yes') : t('generic.no')}</dd></div>
        <div><dt>{t('settings.workflow.dry_run')}</dt><dd>{context.dry_run ? t('generic.yes') : t('generic.no')}</dd></div>
        <div><dt>{t('inventory.run')}</dt><dd>{context.current_run_status ?? '-'}</dd></div>
        {reason && <div><dt>{t('inventory.debug')}</dt><dd>{reason}</dd></div>}
      </dl>
    </details>
  );
}
