// Only locales that have (or will have within this release) a full UI
// translation are exposed in the language picker. Anything else would render
// as an English fallback in most of the UI, which the long ISO-639-1 list
// hides behind "Fallback" badges and confuses users. Keep this list in sync
// with `completeLocales` in `../i18n/messages.ts` — entries listed here that
// are not yet in `completeLocales` still fall back to English at runtime.
const iso639_1LanguageTags = [
  'en', 'de', 'fr', 'es', 'it', 'nl', 'pl'
];

export type LanguageOption = {
  tag: string;
  uiName: string;
  nativeName: string;
};

function displayName(tag: string, locale: string) {
  try {
    return new Intl.DisplayNames([locale], { type: 'language' }).of(tag) ?? tag;
  } catch {
    return tag;
  }
}

export function languageOptions(locale = navigator.language || 'en'): LanguageOption[] {
  return iso639_1LanguageTags
    .map((tag) => ({
      tag,
      uiName: displayName(tag, locale),
      nativeName: displayName(tag, tag)
    }))
    .sort((left, right) => left.uiName.localeCompare(right.uiName, locale));
}

export function languageOptionLabel(option: LanguageOption) {
  if (option.uiName === option.nativeName) {
    return `${option.uiName} (${option.tag})`;
  }
  return `${option.uiName} · ${option.nativeName} (${option.tag})`;
}
