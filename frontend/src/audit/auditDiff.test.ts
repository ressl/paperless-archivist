import { describe, expect, it } from 'vitest';
import { diffAuditSnapshots, formatAuditValue } from './auditDiff';

describe('diffAuditSnapshots (#448)', () => {
  it('reports changed, added and removed fields and skips unchanged ones', () => {
    const rows = diffAuditSnapshots(
      { title: 'Old', correspondent: 7, tags: [1, 2], custom: { iban: 'A', note: 'same' } },
      { title: 'New', document_type: 3, tags: [1, 2], custom: { iban: 'B', note: 'same' } }
    );
    expect(rows).toEqual([
      { path: 'correspondent', kind: 'removed', before: 7 },
      { path: 'custom.iban', kind: 'changed', before: 'A', after: 'B' },
      { path: 'document_type', kind: 'added', after: 3 },
      { path: 'title', kind: 'changed', before: 'Old', after: 'New' }
    ]);
  });

  it('treats arrays as single values and handles missing snapshots', () => {
    expect(diffAuditSnapshots({ tags: [1, 2] }, { tags: [2, 1] })).toEqual([
      { path: 'tags', kind: 'changed', before: [1, 2], after: [2, 1] }
    ]);
    expect(diffAuditSnapshots(null, { enabled: true })).toEqual([{ path: 'enabled', kind: 'added', after: true }]);
    expect(diffAuditSnapshots({ enabled: true }, undefined)).toEqual([
      { path: 'enabled', kind: 'removed', before: true }
    ]);
    expect(diffAuditSnapshots({ a: 1 }, { a: 1 })).toEqual([]);
  });

  it('formats values compactly', () => {
    expect(formatAuditValue('text')).toBe('text');
    expect(formatAuditValue([1, 2])).toBe('[1,2]');
    expect(formatAuditValue(null)).toBe('null');
    expect(formatAuditValue(undefined)).toBe('');
  });
});
