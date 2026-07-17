import { describe, expect, it } from 'vitest';

import {
  SGLANG_MINIMAX_M3_PROVIDER_NAME,
  SGLANG_MINIMAX_M3_TUNING
} from '../modelCatalog';
import { TUNING_PRESETS, tuningPresetKindFor } from './sections/tuning';

describe('SGLang MiniMax M3 measured tuning preset', () => {
  it('uses the dedicated preset only for the exact built-in provider identity', () => {
    expect(tuningPresetKindFor({
      kind: 'openai_compatible',
      name: SGLANG_MINIMAX_M3_PROVIDER_NAME,
      base_url: ''
    })).toBe('sglang_minimax_m3');
    expect(tuningPresetKindFor({
      kind: 'openai_compatible',
      name: 'other-sglang',
      base_url: ''
    })).toBe('openai_compatible');
  });

  it('keeps provider creation and reset-to-defaults values identical', () => {
    expect(TUNING_PRESETS.sglang_minimax_m3).toEqual(SGLANG_MINIMAX_M3_TUNING);
    expect(TUNING_PRESETS.sglang_minimax_m3).toMatchObject({
      worker_concurrency: 1,
      reasoning_effort: null,
      max_output_tokens: 4096,
      structured_output: 'auto',
      request_timeout_seconds: 180
    });
  });
});
