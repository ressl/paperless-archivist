import { useCallback, useEffect, useRef, useState } from 'react';
import {
  api,
  Counts,
  DashboardLiveStatus,
  DashboardRange,
  DashboardStats,
  RecoveryCandidate
} from '../api/client';
import { useI18n } from '../i18n/I18nProvider';
import { localizedErrorMessage } from '../lib/ui';

const DASHBOARD_REFRESH_INTERVAL_MS = 30_000;
const LIVE_REFRESH_INTERVAL_MS = 5_000;

// Runs `tick` immediately, then on `intervalMs` while the page is visible.
// Pauses on `visibilitychange` -> hidden, force-refreshes once on -> visible,
// and tears the interval down cleanly on unmount.
function useVisibleInterval(tick: () => void, intervalMs: number) {
  const tickRef = useRef(tick);
  useEffect(() => {
    tickRef.current = tick;
  }, [tick]);

  useEffect(() => {
    let timer: number | null = null;
    const start = () => {
      if (timer != null) return;
      timer = window.setInterval(() => {
        tickRef.current();
      }, intervalMs);
    };
    const stop = () => {
      if (timer != null) {
        window.clearInterval(timer);
        timer = null;
      }
    };
    const handleVisibility = () => {
      if (typeof document === 'undefined') return;
      if (document.hidden) {
        stop();
      } else {
        // Immediate refresh on return so the dashboard isn't stale.
        tickRef.current();
        start();
      }
    };

    // Initial fire and start.
    tickRef.current();
    if (typeof document === 'undefined' || !document.hidden) {
      start();
    }
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', handleVisibility);
    }
    return () => {
      stop();
      if (typeof document !== 'undefined') {
        document.removeEventListener('visibilitychange', handleVisibility);
      }
    };
  }, [intervalMs]);
}

const DEFAULT_COUNTS: Counts = {
  total_documents: 0,
  complete: 0,
  missing_ocr: 0,
  missing_tagging: 0,
  missing_title: 0,
  missing_correspondent: 0,
  missing_document_type: 0,
  missing_document_date: 0,
  missing_fields: 0,
  waiting_review: 0,
  failed: 0,
  running: 0,
  never_processed: 0
};

export type DashboardStatsState = {
  stats: DashboardStats | null;
  counts: Counts;
  lastLoadedAt: string | null;
  reload: () => Promise<void>;
  setStats: (updater: (current: DashboardStats | null) => DashboardStats | null) => void;
};

export function useDashboardStats(
  range: DashboardRange,
  setError: (message: string | null) => void
): DashboardStatsState {
  const { t } = useI18n();
  const [stats, setStats] = useState<DashboardStats | null>(null);
  const [counts, setCounts] = useState<Counts>(DEFAULT_COUNTS);
  const [lastLoadedAt, setLastLoadedAt] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      const data = await api.dashboard(range);
      setCounts(data.counts);
      setStats(data.stats);
      setLastLoadedAt(new Date().toISOString());
    } catch (err) {
      setError(localizedErrorMessage(err, t));
    }
  }, [range, setError, t]);

  useVisibleInterval(reload, DASHBOARD_REFRESH_INTERVAL_MS);

  const updateStats = useCallback(
    (updater: (current: DashboardStats | null) => DashboardStats | null) => {
      setStats((current) => updater(current));
    },
    []
  );

  return { stats, counts, lastLoadedAt, reload, setStats: updateStats };
}

export type DashboardLiveState = {
  live: DashboardLiveStatus | null;
  recovery: { older_than_seconds: number; items: RecoveryCandidate[] } | null;
  reload: () => Promise<void>;
  reloadRecovery: () => Promise<void>;
  setLive: (updater: (current: DashboardLiveStatus | null) => DashboardLiveStatus | null) => void;
};

export function useDashboardLive(
  // Recovery visibility is now gated on the `ReadRuns` permission instead of a
  // hardcoded admin role check: see issue #98. The server enforces ReadRuns on
  // `/operations/recovery`, so the frontend mirrors the same gate.
  canReadRuns: boolean,
  setError: (message: string | null) => void
): DashboardLiveState {
  const { t } = useI18n();
  const [live, setLive] = useState<DashboardLiveStatus | null>(null);
  const [recovery, setRecovery] = useState<{ older_than_seconds: number; items: RecoveryCandidate[] } | null>(null);

  const reload = useCallback(async () => {
    try {
      const data = await api.dashboardLive();
      setLive(data);
    } catch (err) {
      setError(localizedErrorMessage(err, t));
    }
  }, [setError, t]);

  const reloadRecovery = useCallback(async () => {
    try {
      const data = await api.recoveryStatus();
      setRecovery(data);
    } catch (err) {
      setError(localizedErrorMessage(err, t));
    }
  }, [setError, t]);

  const tick = useCallback(() => {
    void reload();
    if (canReadRuns) void reloadRecovery();
  }, [reload, reloadRecovery, canReadRuns]);

  useVisibleInterval(tick, LIVE_REFRESH_INTERVAL_MS);

  const updateLive = useCallback(
    (updater: (current: DashboardLiveStatus | null) => DashboardLiveStatus | null) => {
      setLive((current) => updater(current));
    },
    []
  );

  return { live, recovery, reload, reloadRecovery, setLive: updateLive };
}

export type FreshnessState = {
  nextRefreshIn: number;
  pulse: boolean;
};

export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState<boolean>(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return false;
    return window.matchMedia(query).matches;
  });
  useEffect(() => {
    if (typeof window === 'undefined' || !window.matchMedia) return;
    const mql = window.matchMedia(query);
    const listener = (event: MediaQueryListEvent) => setMatches(event.matches);
    setMatches(mql.matches);
    mql.addEventListener('change', listener);
    return () => mql.removeEventListener('change', listener);
  }, [query]);
  return matches;
}

export function useFreshness(intervalMs: number, lastLoadedAt: string | null): FreshnessState {
  const [now, setNow] = useState(() => Date.now());
  const lastTickRef = useRef<number>(Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => {
      const ts = Date.now();
      setNow(ts);
      lastTickRef.current = ts;
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);
  const lastLoadedMs = lastLoadedAt ? new Date(lastLoadedAt).getTime() : null;
  const elapsed = lastLoadedMs ? Math.max(0, now - lastLoadedMs) : 0;
  const nextRefreshIn = lastLoadedMs ? Math.max(0, Math.round((intervalMs - elapsed) / 1000)) : Math.round(intervalMs / 1000);
  const pulse = elapsed < 1500;
  return { nextRefreshIn, pulse };
}
