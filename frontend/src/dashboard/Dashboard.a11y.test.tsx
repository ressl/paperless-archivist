import { describe, expect, it } from 'vitest';
import { render, cleanup } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { I18nProvider } from '../i18n/I18nProvider';
import { Status } from '../lib/ui';

expect.extend(toHaveNoViolations);

function renderWithI18n(ui: React.ReactElement) {
  return render(<I18nProvider>{ui}</I18nProvider>);
}

describe('dashboard a11y smoke', () => {
  it('status pill has no critical axe violations', async () => {
    const { container } = renderWithI18n(<Status value="succeeded" />);
    const results = await axe(container, {
      rules: {
        region: { enabled: false }
      }
    });
    expect(results).toHaveNoViolations();
    cleanup();
  });

  it('status pill exposes its label as text and is not a live region (#425)', async () => {
    const { container, getAllByText, queryByRole } = renderWithI18n(
      <div>
        {Array.from({ length: 20 }, (_, index) => (
          <Status key={index} value={index % 2 ? 'failed' : 'succeeded'} />
        ))}
      </div>
    );
    expect(getAllByText('Failed', { selector: '.status-label' })).toHaveLength(10);
    expect(queryByRole('status')).toBeNull();
    expect(container.querySelector('[aria-live], [role="status"], [role="alert"]')).toBeNull();
    cleanup();
  });

  it('multiple status tones render without violations', async () => {
    const { container } = renderWithI18n(
      <div>
        <Status value="succeeded" />
        <Status value="failed" />
        <Status value="running" />
        <Status value="waiting_review" />
        <Status value="queued" />
      </div>
    );
    const results = await axe(container, {
      rules: {
        region: { enabled: false }
      }
    });
    expect(results).toHaveNoViolations();
    cleanup();
  });
});
