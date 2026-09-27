import { describe, expect, it } from 'vitest';
import { diffOutputs, flattenOutput } from './outputDiff';

describe('prompt output diff (#446)', () => {
  it('flattens nested objects to dotted paths and keeps arrays as leaves', () => {
    const flat = flattenOutput({ suggestion: { title: { title: 'A', confidence: 0.9 }, tags: ['x', 'y'] } });
    expect(Object.fromEntries(flat)).toEqual({
      'suggestion.title.title': '"A"',
      'suggestion.title.confidence': '0.9',
      'suggestion.tags': '["x","y"]'
    });
    expect(Object.fromEntries(flattenOutput('plain'))).toEqual({ '(root)': '"plain"' });
  });

  it('reports changed, added and removed fields only', () => {
    const rows = diffOutputs(
      { suggestion: { title: 'A', correspondent: 'ACME', tags: ['x'] } },
      { suggestion: { title: 'B', correspondent: 'ACME', date: '2026-01-01' } }
    );
    expect(rows).toEqual([
      { path: 'suggestion.date', left: null, right: '"2026-01-01"' },
      { path: 'suggestion.tags', left: '["x"]', right: null },
      { path: 'suggestion.title', left: '"A"', right: '"B"' }
    ]);
    expect(diffOutputs({ a: 1 }, { a: 1 })).toEqual([]);
  });
});
