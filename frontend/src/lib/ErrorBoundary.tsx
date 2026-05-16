import { Component, type ErrorInfo, type ReactNode } from 'react';
import { useI18n } from '../i18n/I18nProvider';

type ErrorBoundaryProps = {
  children: ReactNode;
  fallback?: ReactNode;
};

type ErrorBoundaryState = {
  error: Error | null;
};

class ErrorBoundaryInner extends Component<ErrorBoundaryProps & { fallback: ReactNode }, ErrorBoundaryState> {
  constructor(props: ErrorBoundaryProps & { fallback: ReactNode }) {
    super(props);
    this.state = { error: null };
  }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // Log to console so deploys with sourcemaps still expose useful traces.
    // eslint-disable-next-line no-console
    console.error('ErrorBoundary caught render error', error, info.componentStack);
  }

  render() {
    if (this.state.error) return this.props.fallback;
    return this.props.children;
  }
}

export function ErrorBoundary({ children, fallback }: ErrorBoundaryProps) {
  const { t } = useI18n();
  const resolvedFallback = fallback ?? (
    <div className="error-boundary" role="alert">
      <h2>{t('error_boundary.title')}</h2>
      <p>{t('error_boundary.description')}</p>
      <button type="button" className="ghost-button" onClick={() => window.location.reload()}>
        {t('error_boundary.reload')}
      </button>
    </div>
  );
  return <ErrorBoundaryInner fallback={resolvedFallback}>{children}</ErrorBoundaryInner>;
}
