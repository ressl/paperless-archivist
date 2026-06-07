import { Check, RefreshCw, X } from 'lucide-react';
import { useI18n } from '../../i18n/I18nProvider';
import type { ConnectionTestState } from './types';

export function ConnectionTestFeedback({ state }: { state: ConnectionTestState | null }) {
  const { t } = useI18n();
  if (!state) return null;
  return (
    <div
      className={`connection-feedback ${state.status}`}
      role={state.status === 'running' ? 'status' : 'alert'}
      aria-live="polite"
    >
      <header>
        {state.status === 'success' && <Check size={16} />}
        {state.status === 'error' && <X size={16} />}
        {state.status === 'running' && <RefreshCw className="spin" size={16} />}
        <strong>{state.title}</strong>
      </header>
      <p>{state.description}</p>
      {state.hints.length > 0 && (
        <ul>
          {state.hints.map((hint) => (
            <li key={hint}>{hint}</li>
          ))}
        </ul>
      )}
      {state.details && (
        <details>
          <summary>{t('settings.details')}</summary>
          <code>{state.details}</code>
        </details>
      )}
    </div>
  );
}
