import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import { I18nProvider } from '../i18n/I18nProvider';
import { navigate, useLocation } from './router';
import { UnsavedChangesProvider, useUnsavedChangesGuard } from './unsavedChanges';

function DirtyPage({ dirty }: { dirty: boolean }) {
  useUnsavedChangesGuard(dirty);
  const { pathname } = useLocation();
  return <output>{pathname}</output>;
}

function renderGuarded(dirty: boolean) {
  return render(
    <I18nProvider>
      <UnsavedChangesProvider>
        <DirtyPage dirty={dirty} />
      </UnsavedChangesProvider>
    </I18nProvider>
  );
}

function popTo(path: string) {
  act(() => {
    window.history.replaceState(null, '', path);
    window.dispatchEvent(new PopStateEvent('popstate'));
  });
}

beforeEach(() => window.history.replaceState(null, '', '/settings'));
afterEach(() => cleanup());

describe('unsaved-changes guard wired into the router (#423, #424)', () => {
  it('lets navigation through when nothing is dirty', () => {
    renderGuarded(false);
    act(() => {
      navigate('/prompts');
    });
    expect(window.location.pathname).toBe('/prompts');
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument();
  });

  it('asks before an in-app navigation and replays it on discard', () => {
    renderGuarded(true);
    let allowed = true;
    act(() => {
      allowed = navigate('/audit');
    });
    expect(allowed).toBe(false);
    expect(window.location.pathname).toBe('/settings');

    const dialog = screen.getByRole('alertdialog', { name: 'Leave with unsaved changes?' });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Stay on page' }));
    expect(window.location.pathname).toBe('/settings');

    act(() => {
      navigate('/audit');
    });
    fireEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Discard and leave' }));
    expect(window.location.pathname).toBe('/audit');
    expect(screen.getByRole('status')).toHaveTextContent('/audit');
  });

  it('guards browser back/forward: restores the URL, then leaves on discard', () => {
    renderGuarded(true);
    popTo('/');
    expect(window.location.pathname).toBe('/settings');
    expect(screen.getByRole('status')).toHaveTextContent('/settings');
    fireEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Discard and leave' }));
    expect(window.location.pathname).toBe('/');
  });

  it('still blocks reloads via beforeunload', () => {
    renderGuarded(true);
    const event = new Event('beforeunload', { cancelable: true });
    window.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
  });
});
