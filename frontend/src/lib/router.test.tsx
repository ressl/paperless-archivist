import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, render, screen } from '@testing-library/react';
import { navigate, parseRoute, registerBeforeNavigate, replaceLocation, routePath, useBeforeNavigate, useLocation } from './router';

/** Simulate the browser moving to `to` via back/forward. */
function popTo(to: string) {
  act(() => {
    window.history.replaceState(null, '', to);
    window.dispatchEvent(new PopStateEvent('popstate'));
  });
}

function LocationProbe() {
  const { pathname, search } = useLocation();
  return <output>{`${pathname}${search}`}</output>;
}

beforeEach(() => window.history.replaceState(null, '', '/'));
afterEach(() => cleanup());

describe('parseRoute / routePath (#424)', () => {
  it('maps pathnames to tabs and detail ids', () => {
    expect(parseRoute('/')).toEqual({ tab: 'dashboard', known: true });
    expect(parseRoute('/settings')).toEqual({ tab: 'settings', known: true });
    expect(parseRoute('/prompts/')).toEqual({ tab: 'prompts', known: true });
    expect(parseRoute('/reviews/abc-123')).toEqual({ tab: 'reviews', id: 'abc-123', known: true });
    expect(parseRoute('/inventory/42')).toEqual({ tab: 'inventory', id: '42', known: true });
  });

  it('flags unknown paths and detail segments on tabs without details', () => {
    expect(parseRoute('/nope')).toEqual({ tab: 'dashboard', known: false });
    expect(parseRoute('/dashboard')).toEqual({ tab: 'dashboard', known: false });
    expect(parseRoute('/settings/x')).toEqual({ tab: 'settings', known: false });
    expect(parseRoute('/reviews/a/b')).toEqual({ tab: 'reviews', known: false });
    expect(parseRoute('/%E0%A4%A')).toEqual({ tab: 'dashboard', known: false });
  });

  it('builds encoded paths', () => {
    expect(routePath('dashboard')).toBe('/');
    expect(routePath('reviews', 'a b')).toBe('/reviews/a%20b');
    expect(parseRoute(routePath('reviews', 'a b')).id).toBe('a b');
  });
});

describe('navigate / useLocation (#424)', () => {
  it('pushes history entries and re-renders subscribers, including on back', () => {
    render(<LocationProbe />);
    const before = window.history.length;
    act(() => {
      navigate('/inventory?has_error=true');
    });
    expect(window.location.pathname).toBe('/inventory');
    expect(window.history.length).toBe(before + 1);
    expect(screen.getByRole('status')).toHaveTextContent('/inventory?has_error=true');

    popTo('/');
    expect(screen.getByRole('status')).toHaveTextContent(/^\/$/);
  });

  it('replaceLocation rewrites without a new entry and ignores guards', () => {
    render(<LocationProbe />);
    const guard = vi.fn(() => false);
    const unregister = registerBeforeNavigate(guard);
    const before = window.history.length;
    act(() => replaceLocation('/inventory?q=x'));
    expect(window.history.length).toBe(before);
    expect(guard).not.toHaveBeenCalled();
    expect(screen.getByRole('status')).toHaveTextContent('/inventory?q=x');
    unregister();
  });
});

describe('beforeNavigate guards (#424, hook point for #423)', () => {
  it('can veto an in-app navigation', () => {
    render(<LocationProbe />);
    const guard = vi.fn(() => false);
    const unregister = registerBeforeNavigate(guard);
    let result = true;
    act(() => {
      result = navigate('/settings');
    });
    expect(result).toBe(false);
    expect(guard).toHaveBeenCalledWith('/settings');
    expect(window.location.pathname).toBe('/');
    unregister();
    act(() => {
      result = navigate('/settings');
    });
    expect(result).toBe(true);
    expect(window.location.pathname).toBe('/settings');
  });

  it('restores the URL when a guard vetoes browser back/forward', () => {
    render(<LocationProbe />);
    act(() => {
      navigate('/prompts');
    });
    const unregister = registerBeforeNavigate(() => false);
    popTo('/');
    expect(window.location.pathname).toBe('/prompts');
    expect(screen.getByRole('status')).toHaveTextContent('/prompts');
    unregister();
  });

  it('useBeforeNavigate registers while mounted and always calls the latest callback', () => {
    let allow = false;
    function Guarded() {
      useBeforeNavigate(() => allow);
      return null;
    }
    const { unmount } = render(<Guarded />);
    act(() => {
      navigate('/audit');
    });
    expect(window.location.pathname).toBe('/');
    allow = true;
    act(() => {
      navigate('/audit');
    });
    expect(window.location.pathname).toBe('/audit');
    allow = false;
    unmount();
    act(() => {
      navigate('/users');
    });
    expect(window.location.pathname).toBe('/users');
  });
});
