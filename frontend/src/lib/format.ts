import { localizedMessage, type TFunction } from '../i18n/I18nProvider';

export function stageLabel(stage: string, t?: TFunction) {
  const labels: Record<string, string> = {
    ocr: 'OCR',
    ocr_fix: 'OCR Fix',
    title: 'Title',
    document_type: 'Type',
    document_date: 'Date',
    correspondent: 'Correspondent',
    tags: 'Tags',
    fields: 'Fields',
    apply: 'Apply'
  };
  return t ? localizedMessage(`stage.${stage}`, t, labels[stage] ?? statusLabel(stage, t)) : labels[stage] ?? statusLabel(stage);
}

export function statusLabel(value: string, t?: TFunction) {
  if (t) return localizedMessage(`status.${value}`, t, titleCaseStatus(value));
  return titleCaseStatus(value);
}

export function titleCaseStatus(value: string) {
  return value
    .split('_')
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ');
}

export function formatDelta(value: number, t: TFunction, formatNumber: (value: number) => string) {
  if (value === 0) return t('delta.zero');
  return t('delta.value', { value: `${value > 0 ? '+' : ''}${formatNumber(value)}` });
}

export function formatPercent(value: number) {
  if (!Number.isFinite(value)) return '0%';
  return `${Math.round(value * 100)}%`;
}

export function formatRelativeTime(value?: string | null) {
  if (!value) return '-';
  const timestamp = new Date(value).getTime();
  if (!Number.isFinite(timestamp)) return '-';
  const deltaSeconds = Math.round((Date.now() - timestamp) / 1000);
  const future = deltaSeconds < 0;
  const seconds = Math.abs(deltaSeconds);
  if (seconds < 10) return future ? 'in a few seconds' : 'just now';
  if (seconds < 60) return future ? `in ${seconds}s` : `${seconds}s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return future ? `in ${minutes}m` : `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return future ? `in ${hours}h` : `${hours}h ago`;
  return new Date(value).toLocaleString();
}

export function formatMs(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '-';
  if (value >= 1000) return `${(value / 1000).toFixed(1)}s`;
  return `${Math.round(value)}ms`;
}

export function shortId(value: string) {
  return value.slice(0, 8);
}

export function formatCost(value?: number | null) {
  if (value == null) return '-';
  if (value === 0) return '$0.00';
  if (value < 0.01) return `<$0.01`;
  if (value >= 100) return `$${Math.round(value)}`;
  return `$${value.toFixed(2)}`;
}

export function formatMttc(value?: number | null) {
  if (value == null || !Number.isFinite(value) || value <= 0) return '-';
  if (value < 60) return `${Math.round(value)}s`;
  if (value < 3600) {
    const minutes = Math.floor(value / 60);
    const seconds = Math.round(value % 60);
    return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
  }
  const hours = Math.floor(value / 3600);
  const minutes = Math.round((value % 3600) / 60);
  return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
}

// Tone for a change indicator. `higherIsBetter` says whether a rising value is
// good (e.g. throughput) or bad (e.g. failures/backlog), so the colour encodes
// good/bad rather than merely up/down — a rising failure count must read red,
// not green. Pass higherIsBetter=false for "less is better" metrics.
export function deltaTone(value: number, higherIsBetter = true) {
  if (value === 0) return 'delta';
  const isGood = value > 0 ? higherIsBetter : !higherIsBetter;
  const direction = value > 0 ? 'up' : 'down';
  return `delta ${direction} ${isGood ? 'good' : 'bad'}`;
}
