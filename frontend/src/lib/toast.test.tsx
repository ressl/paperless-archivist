import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { I18nProvider } from '../i18n/I18nProvider';
import { MAX_VISIBLE_TOASTS, TOAST_TIMEOUT_MS, ToastProvider, toastReducer, useToast, type Toast, type ToastInput } from './toast';

expect.extend(toHaveNoViolations);

let toastApi: ReturnType<typeof useToast>;
function Capture() {
  toastApi = useToast();
  return null;
}

function renderProvider() {
  return render(
    <I18nProvider>
      <ToastProvider>
        <Capture />
      </ToastProvider>
    </I18nProvider>
  );
}

const notify = (input: ToastInput) => act(() => toastApi.notify(input));

describe('toast stack (#450)', () => {
  beforeEach(() => {
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
  });
  afterEach(() => {
    cleanup();
    vi.useRealTimers();
  });

  it('keeps every message: queues beyond the visible limit and folds repeats', async () => {
    const view = renderProvider();
    const region = screen.getByRole('region', { name: 'Notifications' });
    // Live regions exist before the first message so it is announced.
    expect(within(region).getByRole('alert')).toBeEmptyDOMElement();
    expect(within(region).getByRole('status')).toBeEmptyDOMElement();

    notify({ tone: 'error', message: 'Save failed', scope: 'page' });
    notify({ tone: 'error', message: 'Save failed', scope: 'page' });
    for (let index = 1; index <= MAX_VISIBLE_TOASTS + 1; index += 1) {
      notify({ tone: 'info', message: `Info ${index}` });
    }

    const alert = within(region).getByRole('alert');
    expect(within(alert).getByText('Save failed')).toBeInTheDocument();
    expect(within(alert).getByText('×2')).toBeInTheDocument();
    const status = within(region).getByRole('status');
    // One error + three infos visible, two infos waiting.
    expect(within(status).getAllByText(/^Info \d$/)).toHaveLength(MAX_VISIBLE_TOASTS - 1);
    expect(within(region).getByText('+2 more waiting')).toBeInTheDocument();
    expect(await axe(view.container)).toHaveNoViolations();

    // Dismissing one promotes the next queued message.
    fireEvent.click(within(alert).getByRole('button', { name: 'Dismiss' }));
    expect(within(alert).queryByText('Save failed')).not.toBeInTheDocument();
    expect(within(status).getAllByText(/^Info \d$/)).toHaveLength(MAX_VISIBLE_TOASTS);
    expect(within(region).getByText('+1 more waiting')).toBeInTheDocument();
  });

  it('auto-dismisses non-errors but keeps errors until dismissed', () => {
    vi.useFakeTimers();
    renderProvider();
    notify({ tone: 'success', message: 'Saved' });
    notify({ tone: 'error', message: 'Broken' });
    act(() => vi.advanceTimersByTime(TOAST_TIMEOUT_MS + 10));
    expect(screen.queryByText('Saved')).not.toBeInTheDocument();
    expect(screen.getByText('Broken')).toBeInTheDocument();
  });

  it('pauses auto-dismiss while the pointer is over the stack', () => {
    vi.useFakeTimers();
    renderProvider();
    notify({ tone: 'info', message: 'Read me' });
    fireEvent.mouseEnter(screen.getByRole('region', { name: 'Notifications' }));
    act(() => vi.advanceTimersByTime(TOAST_TIMEOUT_MS * 2));
    expect(screen.getByText('Read me')).toBeInTheDocument();
    fireEvent.mouseLeave(screen.getByRole('region', { name: 'Notifications' }));
    act(() => vi.advanceTimersByTime(TOAST_TIMEOUT_MS + 10));
    expect(screen.queryByText('Read me')).not.toBeInTheDocument();
  });

  it('clears only the requested scope and tone', () => {
    const toast = (id: number, tone: Toast['tone'], scope: string | null): Toast => ({
      id,
      tone,
      scope,
      message: `m${id}`,
      sticky: tone === 'error',
      count: 1
    });
    const state = [toast(1, 'error', 'page'), toast(2, 'success', 'page'), toast(3, 'error', 'budget')];
    expect(toastReducer(state, { type: 'clear', scope: 'page', tone: 'error' }).map((item) => item.id)).toEqual([2, 3]);
    expect(toastReducer(state, { type: 'clear', scope: 'page' }).map((item) => item.id)).toEqual([3]);
  });

  it('is a no-op outside a provider', () => {
    function Outside() {
      const api = useToast();
      api.notify({ tone: 'error', message: 'nowhere' });
      return <span>rendered</span>;
    }
    render(<Outside />);
    expect(screen.getByText('rendered')).toBeInTheDocument();
  });
});
