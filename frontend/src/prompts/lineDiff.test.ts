import { describe, expect, it } from 'vitest';
import { diffLines, lineDiffStats } from './lineDiff';

const lines = (...values: string[]) => values.join('\n');

describe('lineDiff (#435)', () => {
  it('reports identical text as unchanged', () => {
    expect(lineDiffStats(lines('a', 'b'), lines('a', 'b'))).toEqual({ changedLines: 0, addedLines: 0, removedLines: 0 });
  });

  it('counts one inserted line as one addition, not as changes to every following line', () => {
    const before = lines('a', 'b', 'c', 'd', 'e');
    const after = lines('a', 'NEW', 'b', 'c', 'd', 'e');
    expect(lineDiffStats(before, after)).toEqual({ changedLines: 0, addedLines: 1, removedLines: 0 });
    expect(diffLines(before, after).map((op) => op.kind)).toEqual(['equal', 'added', 'equal', 'equal', 'equal', 'equal']);
  });

  it('counts one deleted line as one removal', () => {
    expect(lineDiffStats(lines('a', 'b', 'c', 'd'), lines('b', 'c', 'd'))).toEqual({
      changedLines: 0,
      addedLines: 0,
      removedLines: 1
    });
  });

  it('pairs an in-place edit as a changed line', () => {
    expect(lineDiffStats(lines('a', 'b', 'c'), lines('a', 'B', 'c'))).toEqual({
      changedLines: 1,
      addedLines: 0,
      removedLines: 0
    });
  });

  it('handles mixed hunks and CRLF input', () => {
    const before = 'keep\r\nold one\r\nold two\r\nmiddle\r\ngone\r\nend';
    const after = lines('keep', 'new one', 'middle', 'end', 'extra');
    expect(lineDiffStats(before, after)).toEqual({ changedLines: 1, addedLines: 1, removedLines: 2 });
  });

  it('reconstructs both sides from the operations', () => {
    const before = lines('x', 'a', 'b', 'c', 'y');
    const after = lines('a', 'c', 'b', 'z', 'y');
    const ops = diffLines(before, after);
    expect(ops.filter((op) => op.kind !== 'added').map((op) => op.line)).toEqual(before.split('\n'));
    expect(ops.filter((op) => op.kind !== 'removed').map((op) => op.line)).toEqual(after.split('\n'));
  });
});
