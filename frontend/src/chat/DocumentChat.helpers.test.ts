import { describe, expect, it } from 'vitest';
import { parseDocumentIds } from './DocumentChat';

describe('parseDocumentIds', () => {
  it('returns null for empty input', () => {
    expect(parseDocumentIds('')).toBeNull();
    expect(parseDocumentIds('   ')).toBeNull();
  });

  it('parses a single document id', () => {
    expect(parseDocumentIds('42')).toEqual([42]);
  });

  it('parses a comma-separated list and trims whitespace', () => {
    expect(parseDocumentIds('1, 2 ,3')).toEqual([1, 2, 3]);
  });

  it('deduplicates ids', () => {
    expect(parseDocumentIds('1,2,1,3,2')).toEqual([1, 2, 3]);
  });

  it('rejects non-positive and non-integer ids', () => {
    expect(parseDocumentIds('0')).toBe(false);
    expect(parseDocumentIds('-5')).toBe(false);
    expect(parseDocumentIds('1.5')).toBe(false);
    expect(parseDocumentIds('abc')).toBe(false);
    expect(parseDocumentIds('1,2,abc')).toBe(false);
  });

  it('rejects lists with more than 50 unique ids', () => {
    const big = Array.from({ length: 51 }, (_, idx) => idx + 1).join(',');
    expect(parseDocumentIds(big)).toBe(false);
  });

  it('accepts exactly 50 unique ids', () => {
    const fifty = Array.from({ length: 50 }, (_, idx) => idx + 1).join(',');
    const result = parseDocumentIds(fifty);
    expect(Array.isArray(result)).toBe(true);
    if (Array.isArray(result)) expect(result).toHaveLength(50);
  });
});
