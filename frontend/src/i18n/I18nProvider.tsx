import { createContext, useCallback, useContext, useMemo, useState, type ReactNode } from 'react';
import { languageOptions } from '../data/worldLanguages';
import {
  fallbackLocale,
  hasMessageKey,
  isCompleteLocale,
  messages,
  type CompleteLocale,
  type MessageKey
} from './messages';

const localeStorageKey = 'paperless-archivist.ui-locale';

export type TFunction = (key: MessageKey, values?: Record<string, string | number>) => string;

export type UiLocaleOption = {
  tag: string;
  uiName: string;
  nativeName: string;
  status: 'complete' | 'fallback';
};

type I18nContextValue = {
  locale: string;
  messageLocale: CompleteLocale;
  setLocale: (locale: string) => void;
  localeOptions: UiLocaleOption[];
  t: TFunction;
  formatNumber: (value: number) => string;
  formatPercent: (value: number) => string;
  formatDateTime: (value?: string | null) => string;
  formatRelativeTime: (value?: string | null) => string;
};

const I18nContext = createContext<I18nContextValue | null>(null);

export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocaleState] = useState(() => initialLocale());
  const messageLocale = resolveMessageLocale(locale);

  const t = useCallback<TFunction>(
    (key, values) => interpolate(messages[messageLocale][key] ?? messages[fallbackLocale][key] ?? key, values),
    [messageLocale]
  );

  const setLocale = useCallback((nextLocale: string) => {
    const normalized = normalizeLocale(nextLocale);
    setLocaleState(normalized);
    try {
      window.localStorage.setItem(localeStorageKey, normalized);
    } catch {
      // Locale persistence is nice-to-have; the selected locale still works for the current session.
    }
  }, []);

  const localeOptions = useMemo<UiLocaleOption[]>(
    () =>
      languageOptions(locale).map((option) => {
        const base = baseLanguage(option.tag);
        return {
          ...option,
          status: isCompleteLocale(base) ? 'complete' : 'fallback'
        };
      }),
    [locale]
  );

  const formatNumber = useCallback(
    (value: number) => new Intl.NumberFormat(locale).format(value),
    [locale]
  );

  const formatPercent = useCallback(
    (value: number) =>
      new Intl.NumberFormat(locale, {
        style: 'percent',
        maximumFractionDigits: 0
      }).format(Number.isFinite(value) ? value : 0),
    [locale]
  );

  const formatDateTime = useCallback(
    (value?: string | null) => {
      if (!value) return '-';
      const date = new Date(value);
      if (!Number.isFinite(date.getTime())) return '-';
      return new Intl.DateTimeFormat(locale, {
        dateStyle: 'medium',
        timeStyle: 'short'
      }).format(date);
    },
    [locale]
  );

  const formatRelativeTime = useCallback(
    (value?: string | null) => {
      if (!value) return '-';
      const timestamp = new Date(value).getTime();
      if (!Number.isFinite(timestamp)) return '-';
      const deltaSeconds = Math.round((Date.now() - timestamp) / 1000);
      const future = deltaSeconds < 0;
      const seconds = Math.abs(deltaSeconds);
      if (seconds < 10) return future ? t('time.in_few_seconds') : t('time.just_now');
      if (seconds < 60) {
        return t(future ? 'time.in_seconds' : 'time.seconds_ago', { value: seconds });
      }
      const minutes = Math.round(seconds / 60);
      if (minutes < 60) {
        return t(future ? 'time.in_minutes' : 'time.minutes_ago', { value: minutes });
      }
      const hours = Math.round(minutes / 60);
      if (hours < 24) {
        return t(future ? 'time.in_hours' : 'time.hours_ago', { value: hours });
      }
      return formatDateTime(value);
    },
    [formatDateTime, t]
  );

  const value = useMemo<I18nContextValue>(
    () => ({
      locale,
      messageLocale,
      setLocale,
      localeOptions,
      t,
      formatNumber,
      formatPercent,
      formatDateTime,
      formatRelativeTime
    }),
    [formatDateTime, formatNumber, formatPercent, formatRelativeTime, locale, localeOptions, messageLocale, setLocale, t]
  );

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>;
}

export function useI18n() {
  const value = useContext(I18nContext);
  if (!value) throw new Error('useI18n must be used inside I18nProvider');
  return value;
}

export function resolveMessageLocale(locale: string): CompleteLocale {
  const base = baseLanguage(locale);
  return isCompleteLocale(base) ? base : fallbackLocale;
}

export function localizedMessage(key: string, t: TFunction, fallback: string) {
  return hasMessageKey(key) ? t(key) : fallback;
}

function initialLocale() {
  try {
    const stored = window.localStorage.getItem(localeStorageKey);
    if (stored) return normalizeLocale(stored);
  } catch {
    // Browser storage can be blocked. Fall back to browser language below.
  }
  return normalizeLocale(window.navigator.languages?.[0] ?? window.navigator.language ?? fallbackLocale);
}

function normalizeLocale(value: string) {
  const trimmed = value.trim();
  if (!trimmed) return fallbackLocale;
  try {
    return Intl.getCanonicalLocales(trimmed)[0] ?? fallbackLocale;
  } catch {
    return baseLanguage(trimmed);
  }
}

function baseLanguage(locale: string) {
  return locale.trim().toLowerCase().split('-')[0] || fallbackLocale;
}

function interpolate(template: string, values?: Record<string, string | number>) {
  if (!values) return template;
  return template.replace(/\{(\w+)\}/g, (_, key: string) => String(values[key] ?? `{${key}}`));
}
