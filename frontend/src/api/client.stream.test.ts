import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, STREAM_ERROR_CODE, api, parseSseFrames } from './client';

function sseResponse(chunks: string[], init: ResponseInit = { status: 200 }) {
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    }
  });
  return new Response(body, { headers: { 'content-type': 'text/event-stream' }, ...init });
}

describe('streamed chat client (#449)', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    document.cookie = 'pa_csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT';
  });

  it('parses complete SSE frames and keeps the partial tail', () => {
    const { events, rest } = parseSseFrames(': keep-alive\n\nevent: delta\ndata: {"text":"a"}\n\nevent: del');
    expect(events).toEqual([{ event: 'delta', data: '{"text":"a"}' }]);
    expect(rest).toBe('event: del');
    expect(parseSseFrames('event: x\r\ndata: 1\r\ndata: 2\r\n\r\n').events).toEqual([{ event: 'x', data: '1\n2' }]);
  });

  it('streams sources and deltas split across chunks and resolves with the stored exchange', async () => {
    document.cookie = 'pa_csrf=csrf-value';
    const done = { session_id: 's', user_message_id: 'u', assistant_message_id: 'a', answer: 'Grüße!', sources: [] };
    const fetchMock = vi.fn().mockResolvedValue(
      sseResponse([
        'event: sources\ndata: {"sources":[{"paperless_document_id":3,"snippet":"x","score":1,"source_kind":"k"}]}\n\n',
        'event: delta\ndata: {"text":"Grü',
        'ße"}\n\nevent: delta\ndata: {"text":"!"}\n',
        `\nevent: done\ndata: ${JSON.stringify(done)}\n\n`
      ])
    );
    vi.stubGlobal('fetch', fetchMock);
    const deltas: string[] = [];
    const sources: number[] = [];
    const result = await api.streamChatMessage(
      's',
      { question: 'Hello?' },
      { onDelta: (text) => deltas.push(text), onSources: (items) => sources.push(...items.map((item) => item.paperless_document_id)) }
    );
    expect(result).toEqual(done);
    expect(deltas).toEqual(['Grüße', '!']);
    expect(sources).toEqual([3]);
    const [path, init] = fetchMock.mock.calls[0];
    expect(path).toBe('/api/chat/sessions/s/messages/stream');
    expect(init.method).toBe('POST');
    expect(init.credentials).toBe('include');
    expect((init.headers as Headers).get('x-csrf-token')).toBe('csrf-value');
  });

  it('rejects with the server message for an error event', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(sseResponse(['event: error\ndata: {"error":"provider down"}\n\n'])));
    const error = await api.streamChatMessage('s', { question: 'Hello?' }).catch((err: unknown) => err);
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).message).toBe('provider down');
    expect((error as ApiError).code).toBe(STREAM_ERROR_CODE);
  });

  it('rejects when the stream ends without a done event', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(sseResponse(['event: delta\ndata: {"text":"half"}\n\n'])));
    await expect(api.streamChatMessage('s', { question: 'Hello?' })).rejects.toMatchObject({ code: STREAM_ERROR_CODE });
  });

  it('maps a JSON error before the stream to ApiError with its status', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        new Response(JSON.stringify({ error: 'question must be at least 3 characters' }), {
          status: 400,
          headers: { 'content-type': 'application/json' }
        })
      )
    );
    await expect(api.streamChatMessage('s', { question: '?' })).rejects.toMatchObject({
      status: 400,
      message: 'question must be at least 3 characters'
    });
  });
});
