import assert from 'node:assert/strict';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  DEFAULT_IMAGE_DIGEST,
  DEFAULT_MODEL_REVISION,
  DEFAULT_RUNTIME_REVISION,
  EXACT_MODEL,
} from './sglang_minimax_m3_contract.mjs';
import {
  capacityPayload,
  createCapacityConfig,
  readBounded,
  runCapacitySuite
} from './sglang_minimax_m3_capacity.mjs';

async function withMockServer(handler, callback) {
  const requests = [];
  let active = 0;
  let maxActive = 0;
  const server = createServer(async (request, response) => {
    let raw = '';
    for await (const chunk of request) raw += chunk;
    const body = raw ? JSON.parse(raw) : null;
    active += 1;
    maxActive = Math.max(maxActive, active);
    try {
      requests.push({ headers: request.headers, url: request.url, body });
      const result = await handler({ request, body });
      response.writeHead(result.status, { 'content-type': 'application/json' });
      response.end(result.body);
    } finally {
      active -= 1;
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  try {
    return await callback(
      `http://127.0.0.1:${address.port}`,
      requests,
      () => maxActive
    );
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
}

async function configFor(baseUrl, extra = {}) {
  const directory = await mkdtemp(join(tmpdir(), 'archivist-m3-capacity-'));
  const secretFile = join(directory, 'api-key');
  await writeFile(secretFile, 'capacity-secret\n', { mode: 0o600 });
  const config = createCapacityConfig({
    SGLANG_CAPACITY_BASE_URL: baseUrl,
    SGLANG_CAPACITY_API_KEY_FILE: secretFile,
    SGLANG_CAPACITY_REQUESTS: '4',
    SGLANG_CAPACITY_CONCURRENCY: '2',
    SGLANG_CAPACITY_WARMUP_REQUESTS: '0',
    ...extra
  });
  return { config, cleanup: () => rm(directory, { recursive: true, force: true }) };
}

function successfulResponse(body) {
  const isMetadata = body.response_format?.json_schema?.name === 'archivist_capacity_metadata';
  const content = isMetadata
    ? JSON.stringify({
        title: 'Synthetic capacity document',
        correspondent: 'Synthetic Sender',
        document_type: 'Capacity Test',
        document_date: '2026-01-02',
        tags: ['synthetic']
      })
    : 'ARCHIVIST_CAPACITY_CHAT_OK';
  return {
    status: 200,
    body: JSON.stringify({ choices: [{ message: { content } }] })
  };
}

test('payloads pin M3, disable thinking, bound output, and use only synthetic data', () => {
  const config = {
    model: EXACT_MODEL,
    maxTokens: 4096
  };
  const metadata = capacityPayload('worker_metadata', config, 7);
  const chat = capacityPayload('document_chat', config, 8);

  for (const payload of [metadata, chat]) {
    assert.equal(payload.model, EXACT_MODEL);
    assert.equal(payload.max_tokens, 4096);
    assert.equal(payload.stream, false);
    assert.equal(payload.chat_template_kwargs.thinking_mode, 'disabled');
    const serialized = JSON.stringify(payload);
    assert.match(serialized, /SYNTHETIC-ONLY/);
    assert.doesNotMatch(serialized, /@|https?:\/\/|Bearer|api[_-]?key/i);
  }
  assert.equal(metadata.response_format.type, 'json_schema');
  assert.equal(metadata.response_format.json_schema.strict, true);
  assert.equal(metadata.response_format.json_schema.schema.additionalProperties, false);
  assert.equal(chat.response_format, undefined);
});

test('bounded suite reports sequential, parallel, and mixed aggregate metrics safely', async () => {
  await withMockServer(async ({ body }) => {
    await new Promise((resolve) => setTimeout(resolve, 10));
    return successfulResponse(body);
  }, async (baseUrl, requests, maxActive) => {
    const { config, cleanup } = await configFor(baseUrl);
    try {
      assert.match(config.baseUrl, /\/v1$/);
      const { report, exitCode } = await runCapacitySuite(config);
      assert.equal(exitCode, 0);
      assert.deepEqual(
        report.scenarios.map(({ name }) => name),
        ['sequential_worker_metadata', 'parallel_worker_metadata', 'mixed_worker_metadata_document_chat']
      );
      assert.ok(report.scenarios.every(({ request_count }) => request_count === 4));
      assert.ok(report.scenarios.every(({ success_count }) => success_count === 4));
      assert.ok(report.scenarios.every(({ error_rate }) => error_rate === 0));
      assert.ok(report.scenarios.every(({ timeout_rate }) => timeout_rate === 0));
      assert.ok(report.scenarios.every(({ p50_latency_ms, p95_latency_ms }) =>
        p50_latency_ms > 0 && p95_latency_ms >= p50_latency_ms));
      assert.ok(report.scenarios.every(({ throughput_requests_per_second }) =>
        throughput_requests_per_second > 0));
      assert.ok(maxActive() <= 2);
      assert.equal(requests.length, 12);
      assert.ok(requests.every(({ headers }) => headers.authorization === 'Bearer capacity-secret'));
      assert.ok(requests.every(({ url }) => url === '/v1/chat/completions'));
      const serialized = JSON.stringify(report);
      assert.doesNotMatch(serialized, /capacity-secret|SYNTHETIC-ONLY|choices|127\.0\.0\.1/);
      assert.match(report.target.endpoint_sha256, /^[a-f0-9]{64}$/);
      assert.equal(report.target.model, EXACT_MODEL);
      assert.equal(report.target.model_revision, DEFAULT_MODEL_REVISION);
      assert.equal(report.target.runtime_revision, DEFAULT_RUNTIME_REVISION);
      assert.equal(report.target.image_digest, DEFAULT_IMAGE_DIGEST);
    } finally {
      await cleanup();
    }
  });
});

test('timeouts and response failures are aggregated without response bodies', async () => {
  let calls = 0;
  const controlledFetch = async () => {
    calls += 1;
    if (calls % 2 === 0) {
      const error = new Error('controlled timeout');
      error.name = 'TimeoutError';
      throw error;
    }
    return new Response(JSON.stringify({ error: 'private-response-body' }), { status: 503 });
  };
  const { config, cleanup } = await configFor('https://capacity.example.invalid', {
    SGLANG_CAPACITY_REQUESTS: '2'
  });
  try {
    const { report, exitCode } = await runCapacitySuite(config, {
      fetchImpl: controlledFetch
    });
    assert.equal(exitCode, 1);
    assert.equal(calls, 6);
    assert.ok(report.scenarios.every(({ error_count }) => error_count === 2));
    assert.ok(report.scenarios.every(({ timeout_count }) => timeout_count === 1));
    assert.ok(report.scenarios.every(({ error_rate }) => error_rate === 1));
    assert.ok(report.scenarios.every(({ timeout_rate }) => timeout_rate === 0.5));
    assert.doesNotMatch(JSON.stringify(report), /private-response-body/);
    assert.deepEqual(report.scenarios[0].error_classes, { http_5xx: 1, timeout: 1 });
  } finally {
    await cleanup();
  }
});

test('metadata schema type violations count as contract failures', async () => {
  await withMockServer(async () => ({
    status: 200,
    body: JSON.stringify({
      choices: [{
        message: {
          content: JSON.stringify({
            title: 42,
            correspondent: 'Synthetic Sender',
            document_type: 'Capacity Test',
            document_date: '2026-01-02',
            tags: ['synthetic', 7]
          })
        }
      }]
    })
  }), async (baseUrl) => {
    const { config, cleanup } = await configFor(baseUrl, {
      SGLANG_CAPACITY_REQUESTS: '1',
      SGLANG_CAPACITY_CONCURRENCY: '1'
    });
    try {
      const { report, exitCode } = await runCapacitySuite(config);
      assert.equal(exitCode, 1);
      assert.ok(report.scenarios.every(({ error_classes }) =>
        error_classes.invalid_contract === 1));
    } finally {
      await cleanup();
    }
  });
});

test('declared oversized responses cancel their body before fast failure', async () => {
  let cancelled = false;
  const body = new ReadableStream({
    pull(controller) {
      controller.enqueue(new TextEncoder().encode('{'));
    },
    cancel() {
      cancelled = true;
    }
  });
  const response = new Response(body, {
    headers: { 'content-length': '4096' }
  });
  await assert.rejects(() => readBounded(response, 512), /response_too_large/);
  assert.equal(cancelled, true);
});

test('configuration rejects unsafe bounds, plain secrets, and model overrides', () => {
  const base = { SGLANG_CAPACITY_BASE_URL: 'https://capacity.example.invalid' };
  assert.throws(
    () => createCapacityConfig({ ...base, SGLANG_CAPACITY_REQUESTS: '51' }),
    /SGLANG_CAPACITY_REQUESTS.*between 1 and 50/
  );
  assert.throws(
    () => createCapacityConfig({ ...base, SGLANG_CAPACITY_CONCURRENCY: '17' }),
    /SGLANG_CAPACITY_CONCURRENCY.*between 1 and 16/
  );
  assert.throws(
    () => createCapacityConfig({
      ...base,
      SGLANG_CAPACITY_REQUESTS: '2',
      SGLANG_CAPACITY_CONCURRENCY: '3'
    }),
    /concurrency.*must not exceed.*requests/i
  );
  assert.throws(
    () => createCapacityConfig({ ...base, SGLANG_CAPACITY_MODEL: 'other/model' }),
    /pinned MiniMax M3 model/
  );
  assert.throws(
    () => createCapacityConfig({
      ...base,
      SGLANG_CAPACITY_RUNTIME_REVISION: 'unreviewed-runtime'
    }),
    /must equal the pinned SGLang runtime revision/i
  );
  const exposed = 'plain-secret-value';
  assert.throws(
    () => createCapacityConfig({ ...base, SGLANG_CAPACITY_API_KEY_FILE: exposed }),
    (error) => {
      assert.match(error.message, /readable.*file variable/i);
      assert.doesNotMatch(error.message, new RegExp(exposed));
      return true;
    }
  );
});
