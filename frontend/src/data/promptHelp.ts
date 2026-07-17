import type { Stage } from '../api/client';
import type { MessageKey } from '../i18n/messages';

type PromptStageHelpDefinition = {
  stage: Stage;
  labelKey: MessageKey;
  shortLabelKey: MessageKey;
  purposeKey: MessageKey;
  expectedOutputKey: MessageKey;
  safetyKeys: readonly MessageKey[];
  exampleKeys: readonly MessageKey[];
};

export type PromptStageHelp = {
  stage: Stage;
  label: string;
  shortLabel: string;
  purpose: string;
  expectedOutput: string;
  safety: string[];
  examples: string[];
};

type PromptHelpTranslator = (key: MessageKey) => string;

// As of v1.5.x the only stages with prompts the worker still runs are `ocr` and the
// consolidated `metadata` stage (which replaced the six legacy per-field stages).
export const promptStageOrder = ['ocr', 'metadata'] as const satisfies readonly Stage[];

export const promptStageHelpKeys: Record<Stage, PromptStageHelpDefinition> = {
  ocr: {
    stage: 'ocr',
    labelKey: 'prompts.help.ocr.label',
    shortLabelKey: 'prompts.help.ocr.short_label',
    purposeKey: 'prompts.help.ocr.purpose',
    expectedOutputKey: 'prompts.help.ocr.expected_output',
    safetyKeys: [
      'prompts.help.ocr.safety.preserve',
      'prompts.help.ocr.safety.no_invention',
      'prompts.help.ocr.safety.untrusted'
    ],
    exampleKeys: ['prompts.help.ocr.example.illegible', 'prompts.help.ocr.example.identifiers']
  },
  metadata: {
    stage: 'metadata',
    labelKey: 'prompts.help.metadata.label',
    shortLabelKey: 'prompts.help.metadata.short_label',
    purposeKey: 'prompts.help.metadata.purpose',
    expectedOutputKey: 'prompts.help.metadata.expected_output',
    safetyKeys: [
      'prompts.help.metadata.safety.requested_keys',
      'prompts.help.metadata.safety.allowed_values',
      'prompts.help.metadata.safety.preserve',
      'prompts.help.metadata.safety.untrusted'
    ],
    exampleKeys: [
      'prompts.help.metadata.example.title',
      'prompts.help.metadata.example.omit_tags'
    ]
  }
};

export function resolvePromptStageHelp(stage: Stage, t: PromptHelpTranslator): PromptStageHelp {
  const definition = promptStageHelpKeys[stage];
  return {
    stage: definition.stage,
    label: t(definition.labelKey),
    shortLabel: t(definition.shortLabelKey),
    purpose: t(definition.purposeKey),
    expectedOutput: t(definition.expectedOutputKey),
    safety: definition.safetyKeys.map(t),
    examples: definition.exampleKeys.map(t)
  };
}
