import { type InventoryItem, type InventoryQueryParams, type Stage } from '../api/client';
import type { languageOptions } from '../data/worldLanguages';

export const PAGE_SIZE = 500;
// Default stage set for a bulk re-run: the full business pipeline. Re-running a
// "succeeded-but-wrong" document should regenerate OCR + metadata from scratch.
export const RERUN_STAGES: Stage[] = ['ocr', 'metadata'];
export const STATUS_OPTIONS = ['queued', 'running', 'succeeded', 'failed', 'waiting_review', 'unknown'] as const;
export const RUN_STATUS_OPTIONS = ['queued', 'running', 'waiting_review', 'applying', 'succeeded', 'failed'] as const;

export type Filters = {
  id?: number;
  q?: string;
  ocr_status: string[];
  metadata_status: string[];
  run_status: string[];
  tags_include: string[];
  tags_exclude: string[];
  language?: string;
  date_from?: string;
  date_to?: string;
  has_error?: boolean;
  needs_review?: boolean;
};

export const EMPTY_FILTERS: Filters = {
  ocr_status: [],
  metadata_status: [],
  run_status: [],
  tags_include: [],
  tags_exclude: [],
};

export function parseFiltersFromUrl(): Filters {
  const sp = new URLSearchParams(window.location.search);
  const csv = (key: string) => {
    const value = sp.get(key);
    if (!value) return [];
    return value.split(',').map((s) => s.trim()).filter(Boolean);
  };
  const idValue = sp.get('id');
  const hasError = sp.get('has_error');
  const needsReview = sp.get('needs_review');
  return {
    id: idValue ? Number(idValue) || undefined : undefined,
    q: sp.get('q') ?? undefined,
    ocr_status: csv('ocr_status'),
    metadata_status: csv('metadata_status'),
    run_status: csv('run_status'),
    tags_include: csv('tag'),
    tags_exclude: csv('not_tag'),
    language: sp.get('lang') ?? undefined,
    date_from: sp.get('date_from') ?? undefined,
    date_to: sp.get('date_to') ?? undefined,
    has_error: hasError === 'true' ? true : hasError === 'false' ? false : undefined,
    needs_review: needsReview === 'true' ? true : needsReview === 'false' ? false : undefined,
  };
}

export function filtersToUrl(filters: Filters): string {
  const sp = new URLSearchParams();
  if (filters.id != null) sp.set('id', String(filters.id));
  if (filters.q) sp.set('q', filters.q);
  if (filters.ocr_status.length) sp.set('ocr_status', filters.ocr_status.join(','));
  if (filters.metadata_status.length) sp.set('metadata_status', filters.metadata_status.join(','));
  if (filters.run_status.length) sp.set('run_status', filters.run_status.join(','));
  if (filters.tags_include.length) sp.set('tag', filters.tags_include.join(','));
  if (filters.tags_exclude.length) sp.set('not_tag', filters.tags_exclude.join(','));
  if (filters.language) sp.set('lang', filters.language);
  if (filters.date_from) sp.set('date_from', filters.date_from);
  if (filters.date_to) sp.set('date_to', filters.date_to);
  if (filters.has_error != null) sp.set('has_error', String(filters.has_error));
  if (filters.needs_review != null) sp.set('needs_review', String(filters.needs_review));
  const qs = sp.toString();
  return qs ? `?${qs}` : '';
}

export function filtersToParams(filters: Filters): InventoryQueryParams {
  return {
    id: filters.id,
    q: filters.q,
    ocr_status: filters.ocr_status.length ? filters.ocr_status : undefined,
    metadata_status: filters.metadata_status.length ? filters.metadata_status : undefined,
    run_status: filters.run_status.length ? filters.run_status : undefined,
    tag: filters.tags_include.length ? filters.tags_include : undefined,
    not_tag: filters.tags_exclude.length ? filters.tags_exclude : undefined,
    lang: filters.language,
    date_from: filters.date_from,
    date_to: filters.date_to,
    has_error: filters.has_error,
    needs_review: filters.needs_review,
  };
}

export function isFiltersEmpty(f: Filters): boolean {
  return (
    f.id == null &&
    !f.q &&
    !f.ocr_status.length &&
    !f.metadata_status.length &&
    !f.run_status.length &&
    !f.tags_include.length &&
    !f.tags_exclude.length &&
    !f.language &&
    !f.date_from &&
    !f.date_to &&
    f.has_error == null &&
    f.needs_review == null
  );
}

export function formatLanguageDetection(item: InventoryItem, languages: ReturnType<typeof languageOptions>) {
  const tag = item.detected_language;
  if (!tag) return '-';
  const option = languages.find((language) => language.tag === tag);
  const label = option ? option.uiName : tag;
  const confidence = item.detected_language_confidence;
  if (confidence == null) return label;
  return `${label} ${Math.round(confidence * 100)}%`;
}
