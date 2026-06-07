import { Check, Info } from 'lucide-react';
import type { RuntimeSettings } from '../../api/client';
import { useI18n, type TFunction } from '../../i18n/I18nProvider';
import { isOllamaCloudProvider } from './helpers';
import type { ModelProviderDescriptor } from './types';

export type FirstRunStep = {
  key: string;
  label: string;
  description: string;
  complete: boolean;
};

export function FirstRunWizard({ steps }: { steps: FirstRunStep[] }) {
  const { t } = useI18n();
  if (steps.every((step) => step.complete)) return null;
  return (
    <section className="first-run-wizard">
      <header>
        <div>
          <strong>{t('settings.first_run.title')}</strong>
          <p>{t('settings.first_run.description')}</p>
        </div>
        <span>
          {steps.filter((step) => step.complete).length}/{steps.length}
        </span>
      </header>
      <div className="first-run-steps">
        {steps.map((step) => (
          <article className={step.complete ? 'complete' : ''} key={step.key}>
            {step.complete ? <Check size={16} /> : <Info size={16} />}
            <div>
              <strong>{step.label}</strong>
              <p>{step.description}</p>
            </div>
          </article>
        ))}
      </div>
    </section>
  );
}

export function firstRunWizardSteps(
  settings: RuntimeSettings,
  savedSettings: RuntimeSettings | null,
  provider: ModelProviderDescriptor,
  t: TFunction
): FirstRunStep[] {
  const saved = savedSettings ?? settings;
  const providerNeedsSecret = provider.kind !== 'ollama' || isOllamaCloudProvider(provider);
  return [
    {
      key: 'admin',
      label: t('settings.first_run.admin.label'),
      description: t('settings.first_run.admin.description'),
      complete: true
    },
    {
      key: 'paperless',
      label: t('settings.first_run.paperless.label'),
      description: t('settings.first_run.paperless.description'),
      complete: Boolean(saved.paperless.token_secret_id && saved.paperless.base_url.trim())
    },
    {
      key: 'provider',
      label: t('settings.first_run.provider.label'),
      description: t('settings.first_run.provider.description'),
      complete: Boolean(
        provider.base_url.trim() &&
          (!providerNeedsSecret || settings.ai.providers.find((entry) => entry.name === provider.name)?.secret_id)
      )
    },
    {
      key: 'language',
      label: t('settings.first_run.language.label'),
      description: t('settings.first_run.language.description'),
      complete: Boolean(settings.tagging.tag_output_language)
    },
    {
      key: 'mode',
      label: t('settings.first_run.mode.label'),
      description: t('settings.first_run.mode.description'),
      complete: Boolean(settings.workflow.mode)
    },
    {
      key: 'test',
      label: t('settings.first_run.test.label'),
      description: t('settings.first_run.test.description'),
      complete: Boolean(saved.paperless.token_secret_id && provider.base_url.trim())
    }
  ];
}
