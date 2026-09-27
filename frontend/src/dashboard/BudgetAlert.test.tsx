import { cleanup, render, screen, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { CostBudgetStatus } from '../api/client';
import { I18nProvider } from '../i18n/I18nProvider';
import { ToastProvider } from '../lib/toast';
import { BudgetAlert } from './BudgetAlert';

expect.extend(toHaveNoViolations);

const budget = (overrides: Partial<CostBudgetStatus>): CostBudgetStatus => ({
  monthly_budget_usd: 50,
  warning_percent: 80,
  month_start: '2026-09-01T00:00:00Z',
  month_to_date_cost_usd: 42,
  percent_used: 84,
  level: 'warning',
  ...overrides
});

function renderAlert(value: CostBudgetStatus | null) {
  return render(
    <I18nProvider>
      <ToastProvider>
        <main>
          <BudgetAlert budget={value} />
        </main>
      </ToastProvider>
    </I18nProvider>
  );
}

describe('<BudgetAlert> (#450)', () => {
  beforeEach(() => {
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
    window.sessionStorage.clear();
  });
  afterEach(cleanup);

  it('shows a visible notice and one toast when the warning threshold is reached', async () => {
    const view = renderAlert(budget({}));
    const notice = screen.getByRole('region', { name: 'Monthly AI budget' });
    expect(notice).toHaveTextContent('reached 84% of the budget');
    expect(notice).toHaveTextContent('$42.00 of $50.00 used this month (84%)');
    const toasts = within(screen.getByRole('region', { name: 'Notifications' })).getByRole('status');
    expect(toasts).toHaveTextContent('AI budget warning: 84% of the monthly budget used.');
    expect(await axe(view.container)).toHaveNoViolations();

    // Polls re-render with the same level: no second toast, also not after a remount.
    view.rerender(
      <I18nProvider>
        <ToastProvider>
          <main>
            <BudgetAlert budget={budget({ month_to_date_cost_usd: 43, percent_used: 86 })} />
          </main>
        </ToastProvider>
      </I18nProvider>
    );
    cleanup();
    renderAlert(budget({}));
    expect(within(screen.getByRole('region', { name: 'Notifications' })).getByRole('status')).toBeEmptyDOMElement();
  });

  it('raises an error toast once the budget is exceeded', () => {
    renderAlert(budget({ level: 'exceeded', month_to_date_cost_usd: 61, percent_used: 122 }));
    expect(screen.getByRole('region', { name: 'Monthly AI budget' })).toHaveTextContent('exceeded the budget');
    expect(screen.getByRole('alert')).toHaveTextContent('AI budget exceeded: $61.00 of $50.00 used this month.');
  });

  it('explains an unknown cost and stays silent while within budget', () => {
    renderAlert(budget({ level: 'unknown', month_to_date_cost_usd: null, percent_used: null }));
    expect(screen.getByRole('region', { name: 'Monthly AI budget' })).toHaveTextContent('no provider has token prices');
    cleanup();
    renderAlert(budget({ level: 'ok', percent_used: 10 }));
    expect(screen.queryByRole('region', { name: 'Monthly AI budget' })).not.toBeInTheDocument();
    cleanup();
    renderAlert(null);
    expect(screen.queryByRole('region', { name: 'Monthly AI budget' })).not.toBeInTheDocument();
  });
});
