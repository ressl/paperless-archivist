/**
 * Field-level diff of two parsed prompt-test outputs for the side-by-side
 * version comparison (#446). Objects are flattened to dotted paths (arrays
 * and scalars are leaves, compared by their JSON form), so a metadata result
 * shows e.g. `suggestion.title.title` as one differing row.
 */
export type OutputDiffRow = { path: string; left: string | null; right: string | null };

const MAX_DEPTH = 6;

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function flattenOutput(value: unknown, prefix = '', out = new Map<string, string>(), depth = 0): Map<string, string> {
  if (isPlainObject(value) && depth < MAX_DEPTH && Object.keys(value).length > 0) {
    for (const [key, child] of Object.entries(value)) {
      flattenOutput(child, prefix ? `${prefix}.${key}` : key, out, depth + 1);
    }
    return out;
  }
  out.set(prefix || '(root)', JSON.stringify(value ?? null));
  return out;
}

/** Rows whose value differs (or exists on one side only), sorted by path. */
export function diffOutputs(left: unknown, right: unknown): OutputDiffRow[] {
  const a = flattenOutput(left);
  const b = flattenOutput(right);
  const paths = Array.from(new Set([...a.keys(), ...b.keys()])).sort();
  return paths
    .filter((path) => a.get(path) !== b.get(path))
    .map((path) => ({ path, left: a.get(path) ?? null, right: b.get(path) ?? null }));
}
