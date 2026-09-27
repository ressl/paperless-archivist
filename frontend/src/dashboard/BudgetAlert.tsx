import { useEffect } from 'react';
import { AlertTriangle, Info } from 'lucide-react';
import type { CostBudgetStatus } from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { useToast } from '../lib/toast';

const ANNOUNCED_KEY = 'paperless-archivist.budget-alert';

/**
 * Monthly AI budget notice (#450). Rendered on the dashboard while the
 * month-to-date cost has reached the warning threshold, exceeded the budget,
 * or cannot be estimated. Reaching a new level also raises one toast per
 * browser session (not on every 30 s poll).
 */
export function BudgetAlert({ budget }: { budget: CostBudgetStatus | null }) {
  const { t, formatNumber } = useI18n();
  const toast = useToast();
  const usd = (value: number) => formatNumber(value, { style: 'currency', currency: 'USD', maximumFractionDigits: 2 });
  const percent = (value: number | null | undefined) =>
    formatNumber((value ?? 0) / 100, { style: 'percent', maximumFractionDigits: 0 });

  const level = budget?.level;
  const announceKey = budget && (level === 'warning' || level === 'exceeded') ? `${budget.month_start}:${level}` : null;
  const toastMessage =
    budget && level === 'exceeded'
      ? t('dashboard.budget.toast_exceeded', {
          cost: usd(budget.month_to_date_cost_usd ?? 0),
          budget: usd(budget.monthly_budget_usd)
        })
      : budget && level === 'warning'
        ? t('dashboard.budget.toast_warning', { percent: percent(budget.percent_used) })
        : null;

  useEffect(() => {
    if (!announceKey || !toastMessage) return;
    try {
      if (window.sessionStorage.getItem(ANNOUNCED_KEY) === announceKey) return;
      window.sessionStorage.setItem(ANNOUNCED_KEY, announceKey);
    } catch {
      // Without sessionStorage the toast may repeat after a reload; harmless.
    }
    toast.notify({ tone: level === 'exceeded' ? 'error' : 'warning', message: toastMessage, scope: 'budget' });
    // toastMessage is derived from announceKey's inputs; re-announce only on a new level/month.
  }, [announceKey]);

  if (!budget || level === 'ok') return null;
  const severity = level === 'exceeded' ? 'critical' : level === 'warning' ? 'warning' : 'info';
  const message =
    level === 'exceeded'
      ? t('dashboard.budget.exceeded')
      : level === 'warning'
        ? t('dashboard.budget.warning', { percent: percent(budget.percent_used) })
        : t('dashboard.budget.unknown');
  return (
    <section className={`budget-alert severity-${severity}`} aria-label={t('dashboard.budget.title')}>
      {level === 'unknown' ? <Info size={16} aria-hidden="true" /> : <AlertTriangle size={16} aria-hidden="true" />}
      <div className="alert-text">
        <strong>{t('dashboard.budget.title')}</strong>
        <span>{message}</span>
        {budget.month_to_date_cost_usd != null && (
          <small>
            {t('dashboard.budget.summary', {
              cost: usd(budget.month_to_date_cost_usd),
              budget: usd(budget.monthly_budget_usd),
              percent: percent(budget.percent_used)
            })}
          </small>
        )}
      </div>
    </section>
  );
}
