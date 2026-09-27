import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, render, renderHook, screen, waitFor } from '@testing-library/react';
import { useResource } from './useResource';

type Deferred<T> = { promise: Promise<T>; resolve: (value: T) => void; reject: (reason: unknown) => void };

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

afterEach(() => cleanup());

describe('useResource (#444)', () => {
  it('tracks loading, data and reload', async () => {
    let calls = 0;
    const { result } = renderHook(() => useResource(async () => ++calls, []));
    expect(result.current.loading).toBe(true);
    await waitFor(() => expect(result.current.data).toBe(1));
    expect(result.current.loading).toBe(false);
    await act(() => result.current.reload());
    expect(result.current.data).toBe(2);
  });

  it('never lets an older response overwrite a newer one', async () => {
    const first = deferred<string>();
    const second = deferred<string>();
    const queue = [first, second];
    const signals: AbortSignal[] = [];
    const { result } = renderHook(() =>
      useResource((signal) => {
        signals.push(signal);
        return queue.shift()!.promise;
      }, [])
    );
    act(() => void result.current.reload());
    expect(signals[0].aborted).toBe(true);

    await act(async () => second.resolve('new'));
    await act(async () => first.resolve('old'));
    expect(result.current.data).toBe('new');
    expect(result.current.loading).toBe(false);
  });

  it('reports only the newest request errors, never aborts', async () => {
    const onError = vi.fn();
    const stale = deferred<string>();
    const fresh = deferred<string>();
    const queue = [stale, fresh];
    const { result } = renderHook(() => useResource(() => queue.shift()!.promise, [], { onError }));
    act(() => void result.current.reload());
    await act(async () => stale.reject(new Error('stale failure')));
    expect(onError).not.toHaveBeenCalled();

    await act(async () => fresh.reject(new Error('fresh failure')));
    expect(onError).toHaveBeenCalledTimes(1);
    expect(result.current.error).toEqual(new Error('fresh failure'));

    const aborted = vi.fn();
    const { result: second, unmount } = renderHook(() =>
      useResource(
        (signal) =>
          new Promise<string>((_resolve, reject) =>
            signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')))
          ),
        [],
        { onError: aborted }
      )
    );
    expect(second.current.loading).toBe(true);
    unmount();
    await Promise.resolve();
    expect(aborted).not.toHaveBeenCalled();
  });

  it('refetches, resets and aborts the previous request when deps change', async () => {
    const signals: Record<string, AbortSignal> = {};
    const pending: Record<string, Deferred<string>> = { a: deferred(), b: deferred() };
    function Probe({ id }: { id: string }) {
      const { data } = useResource(
        (signal) => {
          signals[id] = signal;
          return pending[id].promise;
        },
        [id]
      );
      return <output>{data ?? 'empty'}</output>;
    }
    const { rerender } = render(<Probe id="a" />);
    await act(async () => pending.a.resolve('A'));
    expect(screen.getByRole('status')).toHaveTextContent('A');

    pending.a = deferred();
    rerender(<Probe id="b" />);
    expect(screen.getByRole('status')).toHaveTextContent('empty');
    await act(async () => pending.b.resolve('B'));
    expect(screen.getByRole('status')).toHaveTextContent('B');
    expect(signals.a.aborted).toBe(true);
    expect(signals.b.aborted).toBe(false);
  });

  it('cancel() drops the in-flight response', async () => {
    const pending = deferred<string>();
    const { result } = renderHook(() => useResource(() => pending.promise, []));
    act(() => result.current.cancel());
    await act(async () => pending.resolve('late'));
    expect(result.current.data).toBeUndefined();
    expect(result.current.loading).toBe(false);
  });
});
