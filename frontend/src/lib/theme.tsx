import { useState } from 'react';
import { useI18n } from '../i18n/I18nProvider';

/**
 * Colour theme (#450). The stylesheet follows `prefers-color-scheme` by
 * default; an explicit choice is stored per browser and applied as
 * `<html data-theme="light|dark">`, which the dark-token blocks in
 * `18-theme-dark.css` honour. "System" removes the attribute again.
 */

export type ThemePreference = 'system' | 'light' | 'dark';

export const THEME_STORAGE_KEY = 'paperless-archivist.theme';
const PREFERENCES: ThemePreference[] = ['system', 'light', 'dark'];

function isThemePreference(value: unknown): value is ThemePreference {
  return typeof value === 'string' && (PREFERENCES as string[]).includes(value);
}

export function readThemePreference(): ThemePreference {
  try {
    const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
    return isThemePreference(stored) ? stored : 'system';
  } catch {
    return 'system';
  }
}

export function applyThemePreference(preference: ThemePreference): void {
  const root = document.documentElement;
  if (preference === 'system') {
    delete root.dataset.theme;
  } else {
    root.dataset.theme = preference;
  }
}

export function storeThemePreference(preference: ThemePreference): void {
  applyThemePreference(preference);
  try {
    if (preference === 'system') window.localStorage.removeItem(THEME_STORAGE_KEY);
    else window.localStorage.setItem(THEME_STORAGE_KEY, preference);
  } catch {
    // Storage may be unavailable (private mode); the choice still applies
    // for this page view.
  }
}

/** Apply the stored preference before the first render to avoid a flash. */
export function initTheme(): void {
  if (typeof document !== 'undefined') applyThemePreference(readThemePreference());
}

export function ThemeSelector() {
  const { t } = useI18n();
  const [preference, setPreference] = useState<ThemePreference>(readThemePreference);
  return (
    <label className="language-selector theme-selector">
      <span>{t('theme.label')}</span>
      <select
        value={preference}
        aria-label={t('theme.label')}
        onChange={(event) => {
          const next = event.target.value;
          if (!isThemePreference(next)) return;
          setPreference(next);
          storeThemePreference(next);
        }}
      >
        {PREFERENCES.map((option) => (
          <option key={option} value={option}>
            {t(`theme.${option}`)}
          </option>
        ))}
      </select>
    </label>
  );
}
