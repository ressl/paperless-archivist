import { useEffect, useRef, useSyncExternalStore } from 'react';

/**
 * Minimal History-API router (#424).
 *
 * The backend serves index.html for every unknown non-API path, so plain
 * pathnames work for reloads and deep links. The app only needs "which tab /
 * which detail item", so instead of a routing library this module maps
 * pathnames to tabs, keeps React in sync with `pushState` / `popstate`, and
 * offers a `beforeNavigate` registry so features (e.g. unsaved-changes guards)
 * can veto a navigation — including browser back/forward.
 */

export type Tab =
  | 'dashboard'
  | 'statistics'
  | 'inventory'
  | 'chat'
  | 'reviews'
  | 'settings'
  | 'prompts'
  | 'audit'
  | 'users'
  | 'debug';

const TAB_PATHS: Record<Tab, string> = {
  dashboard: '/',
  statistics: '/statistics',
  inventory: '/inventory',
  chat: '/chat',
  reviews: '/reviews',
  settings: '/settings',
  prompts: '/prompts',
  audit: '/audit',
  users: '/users',
  debug: '/debug'
};

export const TABS = Object.keys(TAB_PATHS) as Tab[];

/** Tabs that accept a detail segment: `/inventory/:documentId`, `/reviews/:reviewId`. */
const DETAIL_TABS: ReadonlySet<Tab> = new Set<Tab>(['inventory', 'reviews']);

export type Route = {
  tab: Tab;
  /** Detail segment, e.g. the review id in `/reviews/<id>`. */
  id?: string;
  /** False when the pathname matched no route (rendered as the dashboard). */
  known: boolean;
};

export function isTab(value: string): value is Tab {
  return Object.prototype.hasOwnProperty.call(TAB_PATHS, value);
}

export function parseRoute(pathname: string): Route {
  let segments: string[];
  try {
    segments = pathname.split('/').filter(Boolean).map(decodeURIComponent);
  } catch {
    return { tab: 'dashboard', known: false };
  }
  if (segments.length === 0) return { tab: 'dashboard', known: true };
  const [head, detail, ...rest] = segments;
  if (head === 'dashboard' || !isTab(head)) return { tab: 'dashboard', known: false };
  if (detail === undefined) return { tab: head, known: true };
  if (rest.length === 0 && DETAIL_TABS.has(head)) return { tab: head, id: detail, known: true };
  return { tab: head, known: false };
}

/** Pathname for a tab (and optional detail id). */
export function routePath(tab: Tab, id?: string | number): string {
  const base = TAB_PATHS[tab];
  return id === undefined || id === '' ? base : `${base}/${encodeURIComponent(String(id))}`;
}

// --- Navigation guards -------------------------------------------------------

/** Return `false` to veto the navigation to `to` (a path + search string). */
export type BeforeNavigate = (to: string) => boolean | void;

const guards = new Set<BeforeNavigate>();

/**
 * Register a callback that runs before every in-app navigation (links, tab
 * switches, back/forward). Returning `false` cancels it. Returns an
 * unregister function.
 */
export function registerBeforeNavigate(guard: BeforeNavigate): () => void {
  guards.add(guard);
  return () => {
    guards.delete(guard);
  };
}

/** React wrapper for {@link registerBeforeNavigate}; always calls the latest `guard`. */
export function useBeforeNavigate(guard: BeforeNavigate | null | undefined): void {
  const latest = useRef(guard);
  latest.current = guard;
  const active = Boolean(guard);
  useEffect(() => {
    if (!active) return;
    return registerBeforeNavigate((to) => latest.current?.(to));
  }, [active]);
}

function navigationAllowed(to: string): boolean {
  for (const guard of Array.from(guards)) {
    if (guard(to) === false) return false;
  }
  return true;
}

// --- Location store ----------------------------------------------------------

const listeners = new Set<() => void>();

/** Path + search; the hash is ignored so in-page anchors never count as navigation. */
function currentLocation(): string {
  return `${window.location.pathname}${window.location.search}`;
}

// Last location React rendered; restored when a guard vetoes back/forward.
let committed = typeof window === 'undefined' ? '/' : currentLocation();

function commit(): void {
  committed = currentLocation();
  listeners.forEach((listener) => listener());
}

function onPopState(): void {
  const to = currentLocation();
  if (to === committed) return;
  if (!navigationAllowed(to)) {
    // The browser already moved; put the vetoed-away-from entry back.
    window.history.pushState(null, '', committed);
    return;
  }
  commit();
}

function subscribe(listener: () => void): () => void {
  if (listeners.size === 0) {
    committed = currentLocation();
    window.addEventListener('popstate', onPopState);
  }
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) window.removeEventListener('popstate', onPopState);
  };
}

/**
 * Navigate to `to` (path, optionally with `?search`). Runs the registered
 * guards first; returns false if one vetoed. `replace` rewrites the current
 * history entry instead of adding one.
 */
export function navigate(to: string, { replace = false }: { replace?: boolean } = {}): boolean {
  if (to === currentLocation()) return true;
  if (!navigationAllowed(to)) return false;
  window.history[replace ? 'replaceState' : 'pushState'](null, '', to);
  commit();
  return true;
}

/**
 * Rewrite the current URL without running guards or adding a history entry.
 * For canonicalising a URL (unknown path → `/`) and for pages mirroring their
 * own state (e.g. inventory filters) into the query string.
 */
export function replaceLocation(to: string): void {
  if (to === currentLocation()) return;
  window.history.replaceState(null, '', `${to}${window.location.hash}`);
  commit();
}

/** Current `{ pathname, search }`, re-rendering on every navigation. */
export function useLocation(): { pathname: string; search: string } {
  const href = useSyncExternalStore(subscribe, currentLocation, () => '/');
  const index = href.indexOf('?');
  return index === -1 ? { pathname: href, search: '' } : { pathname: href.slice(0, index), search: href.slice(index) };
}
