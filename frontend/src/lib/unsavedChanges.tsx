import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactNode
} from 'react';
import { useI18n } from '../i18n/I18nProvider';
import { ConfirmDialog } from './ConfirmDialog';

// Unsaved-changes guard (#423). Pages with a local draft (Settings, the Prompt
// editor) register their dirty flag with `useUnsavedChangesGuard(dirty)`. Any
// navigation that would unmount them (the sidebar tab switch today, a router
// transition later) calls `requestNavigation(proceed)` from
// `useNavigationGuard()`: with no dirty page it proceeds immediately, otherwise
// it asks with the shared ConfirmDialog first. A `beforeunload` handler covers
// reloads and closing the tab.

type UnsavedChangesContextValue = {
  setDirty: (sourceId: string, dirty: boolean) => void;
  hasUnsavedChanges: () => boolean;
  requestNavigation: (proceed: () => void) => void;
};

const UnsavedChangesContext = createContext<UnsavedChangesContextValue | null>(null);

export function UnsavedChangesProvider({ children }: { children: ReactNode }) {
  const { t } = useI18n();
  const dirtySources = useRef(new Set<string>());
  const [pendingNavigation, setPendingNavigation] = useState<(() => void) | null>(null);

  const setDirty = useCallback((sourceId: string, dirty: boolean) => {
    if (dirty) dirtySources.current.add(sourceId);
    else dirtySources.current.delete(sourceId);
  }, []);

  const hasUnsavedChanges = useCallback(() => dirtySources.current.size > 0, []);

  const requestNavigation = useCallback((proceed: () => void) => {
    if (dirtySources.current.size === 0) {
      proceed();
      return;
    }
    // Wrap in a thunk: setState would otherwise invoke `proceed` as an updater.
    setPendingNavigation(() => proceed);
  }, []);

  useEffect(() => {
    const onBeforeUnload = (event: BeforeUnloadEvent) => {
      if (dirtySources.current.size === 0) return;
      event.preventDefault();
      // Legacy browsers only show the prompt when returnValue is set.
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', onBeforeUnload);
    return () => window.removeEventListener('beforeunload', onBeforeUnload);
  }, []);

  const cancel = useCallback(() => setPendingNavigation(null), []);
  const discard = useCallback(() => {
    const proceed = pendingNavigation;
    setPendingNavigation(null);
    // The leaving page unregisters itself on unmount; clear eagerly so a
    // navigation chained from `proceed` is not asked about the same draft twice.
    dirtySources.current.clear();
    proceed?.();
  }, [pendingNavigation]);

  const value = useMemo(
    () => ({ setDirty, hasUnsavedChanges, requestNavigation }),
    [setDirty, hasUnsavedChanges, requestNavigation]
  );

  return (
    <UnsavedChangesContext.Provider value={value}>
      {children}
      {pendingNavigation && (
        <ConfirmDialog
          title={t('unsaved.dialog.title')}
          description={t('unsaved.dialog.description')}
          cancelLabel={t('unsaved.dialog.stay')}
          confirmLabel={t('unsaved.dialog.leave')}
          onCancel={cancel}
          onConfirm={discard}
        />
      )}
    </UnsavedChangesContext.Provider>
  );
}

/** Register this component's unsaved draft with the navigation guard. */
export function useUnsavedChangesGuard(dirty: boolean) {
  const context = useContext(UnsavedChangesContext);
  const sourceId = useId();
  const setDirty = context?.setDirty;
  useEffect(() => {
    if (!setDirty) return;
    setDirty(sourceId, dirty);
  }, [setDirty, sourceId, dirty]);
  useEffect(() => {
    if (!setDirty) return;
    return () => setDirty(sourceId, false);
  }, [setDirty, sourceId]);
}

/**
 * Returns `requestNavigation(proceed)`. Outside a provider (isolated component
 * tests) it proceeds immediately.
 */
export function useNavigationGuard(): (proceed: () => void) => void {
  const context = useContext(UnsavedChangesContext);
  return context?.requestNavigation ?? runImmediately;
}

const runImmediately = (proceed: () => void) => proceed();
