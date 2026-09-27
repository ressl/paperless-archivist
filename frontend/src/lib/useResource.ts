import { useCallback, useEffect, useRef, useState, type DependencyList, type Dispatch, type SetStateAction } from 'react';
import { isAbortError } from '../api/client';

export type ResourceOptions<T> = {
  /** Called with every error of the newest request (never for aborted or superseded ones). */
  onError?: (error: unknown) => void;
  /** Called with the data of the newest request once it has been stored. */
  onSuccess?: (data: T) => void;
};

export type Resource<T> = {
  /** Data of the newest successful request; undefined until the first one lands (and after `deps` change). */
  data: T | undefined;
  /** Error of the newest request, if it failed. */
  error: unknown;
  /** True while the newest request is in flight. */
  loading: boolean;
  /** Re-run the fetcher; resolves once this request settled (or was superseded). */
  reload: () => Promise<void>;
  /** Abort and ignore the in-flight request (e.g. the user moved on before `deps` changed). */
  cancel: () => void;
  /** Local update of the cached data (optimistic inserts etc.). */
  setData: Dispatch<SetStateAction<T | undefined>>;
};

/**
 * Shared fetch state for a page (#444): loading / error / reload with request
 * ownership. Every run gets a fresh AbortSignal and a request id; starting a
 * new run (reload, `deps` change, unmount) aborts the previous one, and only
 * the newest request may write data, error or loading — so a slow, older
 * response can never overwrite a newer one, and errors of superseded requests
 * are not reported.
 *
 * `fetcher` and the callbacks may be inline closures; only `deps` re-triggers
 * the fetch (like `useEffect`).
 */
export function useResource<T>(
  fetcher: (signal: AbortSignal) => Promise<T>,
  deps: DependencyList,
  options: ResourceOptions<T> = {}
): Resource<T> {
  const [data, setData] = useState<T | undefined>(undefined);
  const [error, setError] = useState<unknown>(undefined);
  const [loading, setLoading] = useState(true);

  const latest = useRef({ fetcher, options });
  latest.current = { fetcher, options };
  const requestId = useRef(0);
  const controller = useRef<AbortController | null>(null);

  const load = useCallback(async () => {
    controller.current?.abort();
    const own = new AbortController();
    controller.current = own;
    const id = ++requestId.current;
    const isCurrent = () => requestId.current === id;
    setLoading(true);
    setError(undefined);
    try {
      const result = await latest.current.fetcher(own.signal);
      if (!isCurrent()) return;
      setData(() => result);
      latest.current.options.onSuccess?.(result);
    } catch (err) {
      if (!isCurrent() || isAbortError(err)) return;
      setError(err);
      latest.current.options.onError?.(err);
    } finally {
      if (isCurrent()) setLoading(false);
    }
  }, []);

  const cancel = useCallback(() => {
    requestId.current += 1;
    controller.current?.abort();
    setLoading(false);
  }, []);

  useEffect(() => {
    setData(undefined);
    void load();
    // Supersede and cancel the in-flight request on deps change / unmount.
    return cancel;
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `deps` is the caller's dependency list.
  }, deps);

  return { data, error, loading, reload: load, cancel, setData };
}
