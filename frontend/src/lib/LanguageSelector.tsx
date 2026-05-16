import { useI18n } from '../i18n/I18nProvider';

export function LanguageSelector({ compact }: { compact?: boolean }) {
  const { locale, setLocale, localeOptions, t } = useI18n();
  const selectedBase = locale.toLowerCase().split('-')[0] || 'en';
  const selectedOption = localeOptions.find((option) => option.tag === selectedBase);
  return (
    <label className={`language-selector${compact ? ' compact' : ''}`}>
      <span>{t('language.selector.label')}</span>
      <select
        value={selectedOption?.tag ?? selectedBase}
        aria-label={t('language.selector.label')}
        onChange={(event) => setLocale(event.target.value)}
      >
        {localeOptions.map((option) => (
          <option key={option.tag} value={option.tag}>
            {option.uiName === option.nativeName
              ? option.uiName
              : `${option.uiName} · ${option.nativeName}`}
          </option>
        ))}
      </select>
    </label>
  );
}
