import { useCallback, useEffect, useRef, useState } from 'react';
import {
  api,
  CostBudgetStatus,
  Counts,
  DashboardLiveStatus,
  DashboardRange,
  DashboardStats,
  RecoveryCandidate
} from '../api/client';

const DASHBOARD_REFRESH_INTERVAL_MS = 30_000;
const LIVE_REFRESH_INTERVAL_MS = 5_000;
// Longest pause between live-poll retries while the API keeps failing (#427).
export const LIVE_MAX_BACKOFF_MS = 60_000;

/**
 * Health of a background poll (#427). Poll failures no longer go to the
 * global error banner (which reappeared every 5s, never cleared itself and
 * overwrote other messages); instead the dashboard shows an inline
 * "stale / reconnecting" indicator that clears on the next success. Repeated
 * failures back off exponentially (interval * 2^(failures-1), capped).
 */
export type PollHealth = {
  stale: boolean;
  failures: number;
};

// Background poll failures are reported via PollHealth, never thrown.
const ignorePollError = () => undefined;

function usePollHealth(baseIntervalMs: number) {
  const [health, setHealth] = useState<PollHealth>({ stale: false, failures: 0 });
  const failuresRef = useRef(0);
  const nextAttemptAtRef = useRef(0);

  const shouldSkip = useCallback(() => Date.now() < nextAttemptAtRef.current, []);
  const recordSuccess = useCallback(() => {
    if (failuresRef.current === 0) return;
    failuresRef.current = 0;
    nextAttemptAtRef.current = 0;
    setHealth({ stale: false, failures: 0 });
  }, []);
  const recordFailure = useCallback(() => {
    failuresRef.current += 1;
    const delay = Math.min(baseIntervalMs * 2 ** (failuresRef.current - 1), LIVE_MAX_BACKOFF_MS);
    nextAttemptAtRef.current = Date.now() + delay;
    setHealth({ stale: true, failures: failuresRef.current });
  }, [baseIntervalMs]);

  return { health, shouldSkip, recordSuccess, recordFailure };
}

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
  waiting_review: 0,
  failed: 0,
  running: 0,
  never_processed: 0
};

export type DashboardStatsState = {
  stats: DashboardStats | null;
  counts: Counts;
  lastLoadedAt: string | null;
  /** Month-to-date cost vs. the monthly budget; null when none is set (#450). */
  budget: CostBudgetStatus | null;
  reload: () => Promise<void>;
  setStats: (updater: (current: DashboardStats | null) => DashboardStats | null) => void;
  health: PollHealth;
};

export function useDashboardStats(range: DashboardRange): DashboardStatsState {
  const [stats, setStats] = useState<DashboardStats | null>(null);
  const [counts, setCounts] = useState<Counts>(DEFAULT_COUNTS);
  const [lastLoadedAt, setLastLoadedAt] = useState<string | null>(null);
  const [budget, setBudget] = useState<CostBudgetStatus | null>(null);
  // Monotonic request id so a slow response for an old range can't overwrite
  // the data for a newer one (out-of-order guard).
  const requestIdRef = useRef(0);
  const { health, shouldSkip, recordSuccess, recordFailure } = usePollHealth(DASHBOARD_REFRESH_INTERVAL_MS);

  const reload = useCallback(async () => {
    const requestId = ++requestIdRef.current;
    try {
      const data = await api.dashboard(range);
      if (requestId !== requestIdRef.current) return;
      setCounts(data.counts);
      setStats(data.stats);
      setBudget(data.budget ?? null);
      setLastLoadedAt(new Date().toISOString());
      recordSuccess();
    } catch (err) {
      if (requestId !== requestIdRef.current) return;
      recordFailure();
      // Explicit callers (Refresh button) surface the error; the poll swallows it.
      throw err;
    }
  }, [range, recordFailure, recordSuccess]);

  const pollTick = useCallback(() => {
    if (!shouldSkip()) reload().catch(ignorePollError);
  }, [reload, shouldSkip]);

  useVisibleInterval(pollTick, DASHBOARD_REFRESH_INTERVAL_MS);

  // Refetch immediately when the range changes. useVisibleInterval only fires
  // on mount and on its interval, so without this a range switch shows the
  // previous range's data (under the new label) for up to 30s. The initial
  // mount fetch is already done by useVisibleInterval, so skip the first run.
  const isFirstRangeEffect = useRef(true);
  useEffect(() => {
    if (isFirstRangeEffect.current) {
      isFirstRangeEffect.current = false;
      return;
    }
    reload().catch(ignorePollError);
  }, [reload]);

  const updateStats = useCallback(
    (updater: (current: DashboardStats | null) => DashboardStats | null) => {
      setStats((current) => updater(current));
    },
    []
  );

  return { stats, counts, lastLoadedAt, budget, reload, setStats: updateStats, health };
}

export type DashboardLiveState = {
  live: DashboardLiveStatus | null;
  recovery: { older_than_seconds: number; items: RecoveryCandidate[] } | null;
  reload: () => Promise<void>;
  reloadRecovery: () => Promise<void>;
  setLive: (updater: (current: DashboardLiveStatus | null) => DashboardLiveStatus | null) => void;
  health: PollHealth;
};

export function useDashboardLive(
  // Recovery visibility is now gated on the `ReadRuns` permission instead of a
  // hardcoded admin role check: see issue #98. The server enforces ReadRuns on
  // `/operations/recovery`, so the frontend mirrors the same gate.
  canReadRuns: boolean
): DashboardLiveState {
  const [live, setLive] = useState<DashboardLiveStatus | null>(null);
  const [recovery, setRecovery] = useState<{ older_than_seconds: number; items: RecoveryCandidate[] } | null>(null);

  // Monotonic request sequence per poll: the 5s tick can issue a new request
  // before the previous one resolves, so an older (slower) response must not
  // clobber a newer snapshot. Apply state only if this is still the latest
  // request issued. (#296)
  const liveSeqRef = useRef(0);
  const recoverySeqRef = useRef(0);
  const { health, shouldSkip, recordSuccess, recordFailure } = usePollHealth(LIVE_REFRESH_INTERVAL_MS);

  const reload = useCallback(async () => {
    const seq = ++liveSeqRef.current;
    try {
      const data = await api.dashboardLive();
      if (seq !== liveSeqRef.current) return;
      setLive(data);
      recordSuccess();
    } catch (err) {
      if (seq !== liveSeqRef.current) return;
      recordFailure();
      throw err;
    }
  }, [recordFailure, recordSuccess]);

  // Explicit callers see errors; the background poll swallows them and keeps
  // the last recovery snapshot (#427).
  const reloadRecovery = useCallback(async () => {
    const seq = ++recoverySeqRef.current;
    const data = await api.recoveryStatus();
    if (seq === recoverySeqRef.current) setRecovery(data);
  }, []);

  const tick = useCallback(() => {
    // Back off while the API keeps failing instead of hammering it every 5s.
    if (shouldSkip()) return;
    reload().catch(ignorePollError);
    if (canReadRuns) reloadRecovery().catch(ignorePollError);
  }, [reload, reloadRecovery, canReadRuns, shouldSkip]);

  useVisibleInterval(tick, LIVE_REFRESH_INTERVAL_MS);

  const updateLive = useCallback(
    (updater: (current: DashboardLiveStatus | null) => DashboardLiveStatus | null) => {
      setLive((current) => updater(current));
    },
    []
  );

  return { live, recovery, reload, reloadRecovery, setLive: updateLive, health };
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
