import type { ReactNode } from 'react';

export function Sparkline({
  data,
  label,
  format
}: {
  data: Array<number | null>;
  label: string;
  format?: (value: number) => string;
}) {
  if (data.length === 0) {
    return <span className="sparkline empty" aria-label={label}>—</span>;
  }
  const max = data.reduce<number>((acc, value) => (value != null && value > acc ? value : acc), 0);
  const total = data.reduce<number>((acc, value) => acc + (value ?? 0), 0);
  const description = format ? format(total) : `${total.toFixed(2)}`;
  return (
    <span
      className="sparkline"
      role="img"
      aria-label={`${label}: ${description}`}
      title={`${label}: ${description}`}
    >
      {data.map((value, idx) => {
        const heightPct = max > 0 && value != null ? Math.max(2, (value / max) * 100) : 0;
        const empty = value == null;
        return (
          <span
            key={idx}
            className={empty ? 'sparkline-bar empty' : 'sparkline-bar'}
            style={{ height: `${heightPct}%` }}
          />
        );
      })}
    </span>
  );
}

export function ChartPanel({ title, wide, children }: { title: string; wide?: boolean; children: ReactNode }) {
  return (
    <section className={`card chart-panel${wide ? ' wide' : ''}`} aria-label={title}>
      <h3>{title}</h3>
      {children}
    </section>
  );
}
