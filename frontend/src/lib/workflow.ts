import type { ProcessingMode } from '../api/client';
import type { TFunction } from '../i18n/I18nProvider';

export const workflowModeOptions: Array<{
  value: ProcessingMode;
  labelKey: Parameters<TFunction>[0];
  descriptionKey: Parameters<TFunction>[0];
}> = [
  {
    value: 'manual_review',
    labelKey: 'workflow.mode.manual.label',
    descriptionKey: 'workflow.mode.manual.description'
  },
  {
    value: 'auto_select_review',
    labelKey: 'workflow.mode.auto_select_review.label',
    descriptionKey: 'workflow.mode.auto_select_review.description'
  },
  {
    value: 'full_auto',
    labelKey: 'workflow.mode.full_auto.label',
    descriptionKey: 'workflow.mode.full_auto.description'
  }
];

export const workflowModeLabel = (mode: ProcessingMode, t: TFunction) => {
  const option = workflowModeOptions.find((entry) => entry.value === mode);
  return option ? t(option.labelKey) : mode;
};

export const workflowModeDescription = (mode: ProcessingMode, t: TFunction) => {
  const option = workflowModeOptions.find((entry) => entry.value === mode);
  return option ? t(option.descriptionKey) : '';
};
