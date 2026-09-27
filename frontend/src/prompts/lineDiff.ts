/**
 * Line diff for the prompt compare view (#435). Uses a longest-common-
 * subsequence alignment, so inserting or deleting one line only counts that
 * line instead of marking every following line as changed (which a
 * position-by-position comparison did).
 */

export type LineDiffOp = { kind: 'equal' | 'added' | 'removed'; line: string };

export type LineDiffStats = {
  /** Lines replaced in place (a removal paired with an insertion in the same hunk). */
  changedLines: number;
  /** Pure insertions. */
  addedLines: number;
  /** Pure deletions. */
  removedLines: number;
};

// Above this many LCS cells (after trimming the common prefix/suffix) fall back
// to treating the middle as one replaced block, to keep the UI responsive.
const MAX_LCS_CELLS = 4_000_000;

const splitLines = (text: string) => text.split(/\r?\n/);

export function diffLines(before: string, after: string): LineDiffOp[] {
  const a = splitLines(before);
  const b = splitLines(after);

  let prefix = 0;
  while (prefix < a.length && prefix < b.length && a[prefix] === b[prefix]) prefix += 1;
  let suffix = 0;
  while (
    suffix < a.length - prefix &&
    suffix < b.length - prefix &&
    a[a.length - 1 - suffix] === b[b.length - 1 - suffix]
  ) {
    suffix += 1;
  }

  const head: LineDiffOp[] = a.slice(0, prefix).map((line) => ({ kind: 'equal', line }));
  const tail: LineDiffOp[] = a.slice(a.length - suffix).map((line) => ({ kind: 'equal', line }));
  const midA = a.slice(prefix, a.length - suffix);
  const midB = b.slice(prefix, b.length - suffix);

  if (midA.length * midB.length > MAX_LCS_CELLS) {
    return [
      ...head,
      ...midA.map((line): LineDiffOp => ({ kind: 'removed', line })),
      ...midB.map((line): LineDiffOp => ({ kind: 'added', line })),
      ...tail
    ];
  }

  // lcs[i][j] = LCS length of midA[i..] and midB[j..].
  const rows = midA.length + 1;
  const cols = midB.length + 1;
  const lcs = new Uint32Array(rows * cols);
  for (let i = midA.length - 1; i >= 0; i -= 1) {
    for (let j = midB.length - 1; j >= 0; j -= 1) {
      lcs[i * cols + j] =
        midA[i] === midB[j] ? lcs[(i + 1) * cols + j + 1] + 1 : Math.max(lcs[(i + 1) * cols + j], lcs[i * cols + j + 1]);
    }
  }

  const middle: LineDiffOp[] = [];
  let i = 0;
  let j = 0;
  while (i < midA.length && j < midB.length) {
    if (midA[i] === midB[j]) {
      middle.push({ kind: 'equal', line: midA[i] });
      i += 1;
      j += 1;
    } else if (lcs[(i + 1) * cols + j] >= lcs[i * cols + j + 1]) {
      middle.push({ kind: 'removed', line: midA[i] });
      i += 1;
    } else {
      middle.push({ kind: 'added', line: midB[j] });
      j += 1;
    }
  }
  while (i < midA.length) middle.push({ kind: 'removed', line: midA[i++] });
  while (j < midB.length) middle.push({ kind: 'added', line: midB[j++] });

  return [...head, ...middle, ...tail];
}

export function lineDiffStats(before: string, after: string): LineDiffStats {
  const stats: LineDiffStats = { changedLines: 0, addedLines: 0, removedLines: 0 };
  let hunkAdded = 0;
  let hunkRemoved = 0;
  const flush = () => {
    const changed = Math.min(hunkAdded, hunkRemoved);
    stats.changedLines += changed;
    stats.addedLines += hunkAdded - changed;
    stats.removedLines += hunkRemoved - changed;
    hunkAdded = 0;
    hunkRemoved = 0;
  };
  for (const op of diffLines(before, after)) {
    if (op.kind === 'added') hunkAdded += 1;
    else if (op.kind === 'removed') hunkRemoved += 1;
    else flush();
  }
  flush();
  return stats;
}
