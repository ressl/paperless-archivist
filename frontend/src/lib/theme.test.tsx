import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { axe, toHaveNoViolations } from 'jest-axe';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { I18nProvider } from '../i18n/I18nProvider';
import { THEME_STORAGE_KEY, ThemeSelector, initTheme } from './theme';

expect.extend(toHaveNoViolations);

describe('theme toggle (#450)', () => {
  beforeEach(() => {
    window.localStorage.clear();
    window.localStorage.setItem('paperless-archivist.ui-locale', 'en');
    delete document.documentElement.dataset.theme;
  });
  afterEach(cleanup);

  it('follows the system by default and persists an explicit choice per browser', async () => {
    const view = render(
      <I18nProvider>
        <ThemeSelector />
      </I18nProvider>
    );
    const select = screen.getByRole('combobox', { name: 'Theme' });
    expect(select).toHaveValue('system');
    expect(document.documentElement.dataset.theme).toBeUndefined();
    expect(await axe(view.container)).toHaveNoViolations();

    fireEvent.change(select, { target: { value: 'dark' } });
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe('dark');

    fireEvent.change(select, { target: { value: 'system' } });
    expect(document.documentElement.dataset.theme).toBeUndefined();
    expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBeNull();
  });

  it('applies the stored choice before the first render and ignores junk', () => {
    window.localStorage.setItem(THEME_STORAGE_KEY, 'light');
    initTheme();
    expect(document.documentElement.dataset.theme).toBe('light');
    window.localStorage.setItem(THEME_STORAGE_KEY, 'purple');
    initTheme();
    expect(document.documentElement.dataset.theme).toBeUndefined();
  });
});
