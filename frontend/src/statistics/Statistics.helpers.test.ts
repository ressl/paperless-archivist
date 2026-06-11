import { describe, expect, it } from 'vitest';
import { presetParams } from './Statistics';

// #301: presets must be rolling windows — an RFC3339 `from` anchored at the
// current instant and NO `to`, so the backend defaults the end of the range
// to "now" and the current day is always included. The old bare-date params
// cut the range at today's UTC midnight, hiding everything recorded today.
describe('presetParams', () => {
  const now = new Date('2026-06-11T15:00:00.000Z');

  it('sends a rolling RFC3339 from for the timed presets', () => {
    expect(presetParams('24h', now)).toEqual({ from: '2026-06-10T15:00:00.000Z' });
    expect(presetParams('7d', now)).toEqual({ from: '2026-06-04T15:00:00.000Z' });
    expect(presetParams('30d', now)).toEqual({ from: '2026-05-12T15:00:00.000Z' });
    expect(presetParams('90d', now)).toEqual({ from: '2026-03-13T15:00:00.000Z' });
  });

  it('omits to so the backend default (now) applies', () => {
    for (const preset of ['24h', '7d', '30d', '90d', 'all'] as const) {
      expect(presetParams(preset, now)).not.toHaveProperty('to');
    }
  });

  it('uses the far-past sentinel for all-time', () => {
    expect(presetParams('all', now)).toEqual({ from: '2000-01-01' });
  });
});
