/**
 * Before/after diff for audit events (#448).
 *
 * Both snapshots are flattened to dotted paths (`title`, `tags`, `custom_fields.0.value`
 * for nested objects); arrays and scalars are leaves compared by their JSON
 * form, so a reordered tag list shows as one changed row instead of noise per
 * index. Only differing paths are returned, in stable (sorted) order.
 */
export type AuditDiffKind = 'added' | 'removed' | 'changed';

export type AuditDiffRow = {
  path: string;
  kind: AuditDiffKind;
  before?: unknown;
  after?: unknown;
};

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function flatten(value: unknown, prefix: string, out: Map<string, unknown>) {
  if (isPlainObject(value)) {
    const keys = Object.keys(value);
    if (keys.length === 0 && prefix) out.set(prefix, value);
    for (const key of keys) {
      flatten(value[key], prefix ? `${prefix}.${key}` : key, out);
    }
    return;
  }
  // A non-object snapshot (e.g. a bare string) is shown under an empty path.
  out.set(prefix, value);
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

export function diffAuditSnapshots(before: unknown, after: unknown): AuditDiffRow[] {
  const left = new Map<string, unknown>();
  const right = new Map<string, unknown>();
  if (before !== undefined && before !== null) flatten(before, '', left);
  if (after !== undefined && after !== null) flatten(after, '', right);
  const paths = Array.from(new Set([...left.keys(), ...right.keys()])).sort();
  const rows: AuditDiffRow[] = [];
  for (const path of paths) {
    const inLeft = left.has(path);
    const inRight = right.has(path);
    if (inLeft && inRight) {
      if (!same(left.get(path), right.get(path))) {
        rows.push({ path, kind: 'changed', before: left.get(path), after: right.get(path) });
      }
    } else if (inRight) {
      rows.push({ path, kind: 'added', after: right.get(path) });
    } else {
      rows.push({ path, kind: 'removed', before: left.get(path) });
    }
  }
  return rows;
}

/** Compact display form of a diff value. */
export function formatAuditValue(value: unknown): string {
  if (value === undefined) return '';
  if (typeof value === 'string') return value;
  return JSON.stringify(value);
}
