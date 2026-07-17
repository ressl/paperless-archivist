#!/usr/bin/env node

import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import {
  DEFAULT_IMAGE_DIGEST,
  DEFAULT_MODEL_REVISION,
  DEFAULT_RUNTIME_REVISION,
  EXACT_MODEL
} from './sglang_minimax_m3_contract.mjs';

export { EXACT_MODEL };

const MAX_REQUESTS = 50;
const MAX_CONCURRENCY = 16;
const MAX_RESPONSE_BYTES = 16 * 1024 * 1024;

class CapacityFailure extends Error {
  constructor(errorClass) {
    super(errorClass);
    this.errorClass = errorClass;
  }
}

function capacityUrl(raw) {
  if (!raw?.trim()) throw new Error('SGLANG_CAPACITY_BASE_URL is required');
  try {
    const url = new URL(raw.trim());
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) {
      throw new Error('invalid');
    }
    url.search = '';
    url.hash = '';
    const path = url.pathname.replace(/\/+$/, '');
    url.pathname = path.endsWith('/v1') ? path : `${path}/v1`;
    return url.toString().replace(/\/+$/, '');
  } catch {
    throw new Error('SGLANG_CAPACITY_BASE_URL must be a credential-free HTTP(S) URL');
  }
}

function integerSetting(env, name, fallback, { min = 1, max }) {
  const value = env[name] === undefined ? fallback : Number(env[name]);
  if (!Number.isInteger(value) || value < min || value > max) {
    throw new Error(`${name} must be an integer between ${min} and ${max}`);
  }
  return value;
}

function exactPin(env, name, expected, label) {
  const configured = env[name]?.trim();
  if (configured && configured !== expected) {
    throw new Error(`${name} must equal the pinned ${label}`);
  }
  return expected;
}

export function createCapacityConfig(env = process.env) {
  const baseUrl = capacityUrl(env.SGLANG_CAPACITY_BASE_URL);
  const secretFile = env.SGLANG_CAPACITY_API_KEY_FILE?.trim();
  let apiKey = '';
  if (secretFile) {
    try {
      apiKey = readFileSync(secretFile, 'utf8').trim();
    } catch {
      throw new Error(
        'SGLANG_CAPACITY_API_KEY_FILE must reference a readable GitLab File variable'
      );
    }
    if (!apiKey) throw new Error('SGLANG_CAPACITY_API_KEY_FILE must not be empty');
  }
  const model = env.SGLANG_CAPACITY_MODEL?.trim() || EXACT_MODEL;
  if (model !== EXACT_MODEL) {
    throw new Error('SGLANG_CAPACITY_MODEL must equal the pinned MiniMax M3 model');
  }
  const requests = integerSetting(env, 'SGLANG_CAPACITY_REQUESTS', 8, {
    min: 1,
    max: MAX_REQUESTS
  });
  const concurrency = integerSetting(env, 'SGLANG_CAPACITY_CONCURRENCY', 2, {
    min: 1,
    max: MAX_CONCURRENCY
  });
  if (concurrency > requests) {
    throw new Error('SGLANG_CAPACITY_CONCURRENCY must not exceed SGLANG_CAPACITY_REQUESTS');
  }
  return {
    baseUrl,
    apiKey,
    model,
    modelRevision: exactPin(
      env,
      'SGLANG_CAPACITY_MODEL_REVISION',
      DEFAULT_MODEL_REVISION,
      'MiniMax M3 model revision'
    ),
    runtimeRevision: exactPin(
      env,
      'SGLANG_CAPACITY_RUNTIME_REVISION',
      DEFAULT_RUNTIME_REVISION,
      'SGLang runtime revision'
    ),
    imageDigest: exactPin(
      env,
      'SGLANG_CAPACITY_IMAGE_DIGEST',
      DEFAULT_IMAGE_DIGEST,
      'SGLang image digest'
    ),
    requests,
    concurrency,
    warmupRequests: integerSetting(env, 'SGLANG_CAPACITY_WARMUP_REQUESTS', 1, {
      min: 0,
      max: 4
    }),
    timeoutMs: integerSetting(env, 'SGLANG_CAPACITY_TIMEOUT_MS', 180_000, {
      min: 1,
      max: 900_000
    }),
    maxResponseBytes: integerSetting(
      env,
      'SGLANG_CAPACITY_MAX_RESPONSE_BYTES',
      2 * 1024 * 1024,
      { min: 64, max: MAX_RESPONSE_BYTES }
    ),
    maxTokens: integerSetting(env, 'SGLANG_CAPACITY_MAX_TOKENS', 4096, {
      min: 1,
      max: 65_536
    }),
    reportFile: env.SGLANG_CAPACITY_REPORT_FILE?.trim() || null
  };
}

const metadataSchema = {
  type: 'object',
  properties: {
    title: { type: 'string' },
    correspondent: { type: 'string' },
    document_type: { type: 'string' },
    document_date: { type: 'string' },
    tags: { type: 'array', items: { type: 'string' } }
  },
  required: ['title', 'correspondent', 'document_type', 'document_date', 'tags'],
  additionalProperties: false
};

function syntheticDocument(index) {
  const paragraph =
    `SYNTHETIC-ONLY CAPACITY DOCUMENT ${index}. ` +
    'Reference CAPACITY-2026, issue date 2026-01-02, amount CHF 42.00. ' +
    'This generated text contains no person, account, address, email, or real document data.';
  return Array.from({ length: 48 }, () => paragraph).join('\n');
}

export function capacityPayload(consumer, config, index) {
  const common = {
    model: config.model,
    temperature: 0,
    max_tokens: config.maxTokens,
    stream: false,
    chat_template_kwargs: { thinking_mode: 'disabled' }
  };
  if (consumer === 'worker_metadata') {
    return {
      ...common,
      messages: [
        {
          role: 'system',
          content: 'Extract metadata from the explicitly SYNTHETIC-ONLY document.'
        },
        { role: 'user', content: syntheticDocument(index) }
      ],
      response_format: {
        type: 'json_schema',
        json_schema: {
          name: 'archivist_capacity_metadata',
          strict: true,
          schema: metadataSchema
        }
      }
    };
  }
  if (consumer === 'document_chat') {
    return {
      ...common,
      messages: [
        {
          role: 'system',
          content: 'Answer only from the SYNTHETIC-ONLY document. Never use external facts.'
        },
        {
          role: 'user',
          content:
            `${syntheticDocument(index)}\n\n` +
            'If the synthetic amount is CHF 42.00, reply with exactly ' +
            'ARCHIVIST_CAPACITY_CHAT_OK and no other text.'
        }
      ]
    };
  }
  throw new Error(`unknown capacity consumer: ${consumer}`);
}

export async function readBounded(response, limit) {
  const declared = Number(response.headers.get('content-length'));
  if (Number.isFinite(declared) && declared > limit) {
    try {
      await response.body?.cancel();
    } finally {
      throw new CapacityFailure('response_too_large');
    }
  }
  if (!response.body) return '';
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let total = 0;
  let raw = '';
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > limit) {
      await reader.cancel();
      throw new CapacityFailure('response_too_large');
    }
    raw += decoder.decode(value, { stream: true });
  }
  return raw + decoder.decode();
}

function responseContent(parsed) {
  const content = parsed?.choices?.[0]?.message?.content;
  if (typeof content !== 'string' || !content.trim()) {
    throw new CapacityFailure('invalid_contract');
  }
  return content.trim();
}

function validateResponse(consumer, parsed) {
  const content = responseContent(parsed);
  if (consumer === 'document_chat') {
    if (content !== 'ARCHIVIST_CAPACITY_CHAT_OK') {
      throw new CapacityFailure('invalid_contract');
    }
    return;
  }
  let metadata;
  try {
    metadata = JSON.parse(content);
  } catch {
    throw new CapacityFailure('invalid_contract');
  }
  const expected = metadataSchema.required;
  const stringFields = ['title', 'correspondent', 'document_type', 'document_date'];
  if (
    !metadata ||
    typeof metadata !== 'object' ||
    Array.isArray(metadata) ||
    Object.keys(metadata).some((key) => !expected.includes(key)) ||
    expected.some((key) => !(key in metadata)) ||
    stringFields.some((key) => typeof metadata[key] !== 'string') ||
    !Array.isArray(metadata.tags) ||
    metadata.tags.some((tag) => typeof tag !== 'string')
  ) {
    throw new CapacityFailure('invalid_contract');
  }
}

async function capacityRequest(config, consumer, index, fetchImpl) {
  const headers = { accept: 'application/json', 'content-type': 'application/json' };
  if (config.apiKey) headers.authorization = `Bearer ${config.apiKey}`;
  let response;
  try {
    response = await fetchImpl(`${config.baseUrl}/chat/completions`, {
      method: 'POST',
      headers,
      body: JSON.stringify(capacityPayload(consumer, config, index)),
      signal: AbortSignal.timeout(config.timeoutMs)
    });
  } catch (error) {
    if (error?.name === 'TimeoutError' || error?.name === 'AbortError') {
      throw new CapacityFailure('timeout');
    }
    throw new CapacityFailure('network');
  }
  let raw;
  try {
    raw = await readBounded(response, config.maxResponseBytes);
  } catch (error) {
    if (error?.name === 'TimeoutError' || error?.name === 'AbortError') {
      throw new CapacityFailure('timeout');
    }
    throw error;
  }
  if (!response.ok) {
    if (response.status === 429) throw new CapacityFailure('http_429');
    if (response.status >= 500) throw new CapacityFailure('http_5xx');
    throw new CapacityFailure('http_4xx');
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new CapacityFailure('invalid_json');
  }
  validateResponse(consumer, parsed);
}

async function runBounded(items, concurrency, run) {
  const results = new Array(items.length);
  let next = 0;
  const workers = Array.from({ length: Math.min(concurrency, items.length) }, async () => {
    while (true) {
      const current = next;
      next += 1;
      if (current >= items.length) return;
      const started = performance.now();
      try {
        await run(items[current], current);
        results[current] = {
          ok: true,
          latencyMs: performance.now() - started
        };
      } catch (error) {
        results[current] = {
          ok: false,
          latencyMs: performance.now() - started,
          errorClass: error instanceof CapacityFailure ? error.errorClass : 'unknown'
        };
      }
    }
  });
  await Promise.all(workers);
  return results;
}

function percentile(values, fraction) {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)];
}

async function runScenario(config, name, consumers, concurrency, fetchImpl) {
  const started = performance.now();
  const results = await runBounded(
    consumers,
    concurrency,
    (consumer, index) => capacityRequest(config, consumer, index, fetchImpl)
  );
  const durationMs = performance.now() - started;
  const errors = results.filter(({ ok }) => !ok);
  const errorClasses = {};
  for (const result of errors) {
    errorClasses[result.errorClass] = (errorClasses[result.errorClass] ?? 0) + 1;
  }
  const latencies = results.map(({ latencyMs }) => latencyMs);
  const timeoutCount = errorClasses.timeout ?? 0;
  const consumerCounts = {};
  for (const consumer of consumers) {
    consumerCounts[consumer] = (consumerCounts[consumer] ?? 0) + 1;
  }
  return {
    name,
    request_count: consumers.length,
    concurrency,
    consumer_counts: consumerCounts,
    success_count: consumers.length - errors.length,
    error_count: errors.length,
    timeout_count: timeoutCount,
    error_rate: errors.length / consumers.length,
    timeout_rate: timeoutCount / consumers.length,
    throughput_requests_per_second: Number((consumers.length / (durationMs / 1000)).toFixed(3)),
    p50_latency_ms: Math.max(1, Math.round(percentile(latencies, 0.5))),
    p95_latency_ms: Math.max(1, Math.round(percentile(latencies, 0.95))),
    error_classes: errorClasses
  };
}

export async function runCapacitySuite(config, { fetchImpl = fetch } = {}) {
  const warmups = Array.from(
    { length: config.warmupRequests },
    (_, index) => index % 2 === 0 ? 'worker_metadata' : 'document_chat'
  );
  const warmupResults = await runBounded(
    warmups,
    1,
    (consumer, index) => capacityRequest(config, consumer, -index - 1, fetchImpl)
  );
  const workerOnly = Array(config.requests).fill('worker_metadata');
  const mixed = Array.from(
    { length: config.requests },
    (_, index) => index % 2 === 0 ? 'worker_metadata' : 'document_chat'
  );
  const scenarios = [];
  scenarios.push(await runScenario(
    config,
    'sequential_worker_metadata',
    workerOnly,
    1,
    fetchImpl
  ));
  scenarios.push(await runScenario(
    config,
    'parallel_worker_metadata',
    workerOnly,
    config.concurrency,
    fetchImpl
  ));
  scenarios.push(await runScenario(
    config,
    'mixed_worker_metadata_document_chat',
    mixed,
    config.concurrency,
    fetchImpl
  ));
  const warmupFailures = warmupResults.filter(({ ok }) => !ok).length;
  const failed = warmupFailures > 0 || scenarios.some(({ error_count }) => error_count > 0);
  return {
    report: {
      schema_version: 1,
      generated_at: new Date().toISOString(),
      overall_status: failed ? 'failed' : 'passed',
      target: {
        model: config.model,
        model_revision: config.modelRevision,
        runtime_revision: config.runtimeRevision,
        image_digest: config.imageDigest,
        endpoint_sha256: createHash('sha256').update(config.baseUrl).digest('hex')
      },
      limits: {
        requests_per_scenario: config.requests,
        concurrency: config.concurrency,
        warmup_requests: config.warmupRequests,
        request_timeout_ms: config.timeoutMs,
        max_output_tokens: config.maxTokens,
        max_response_bytes: config.maxResponseBytes
      },
      warmup_failures: warmupFailures,
      scenarios
    },
    exitCode: failed ? 1 : 0
  };
}

function configurationFailureReport(error) {
  return {
    schema_version: 1,
    generated_at: new Date().toISOString(),
    overall_status: 'configuration_failed',
    scenarios: [],
    diagnostic: String(error?.message ?? error)
      .replace(/https?:\/\/[^\s"'<>]+/gi, '[ENDPOINT]')
      .slice(0, 512)
  };
}

async function main() {
  let config;
  let report;
  let exitCode;
  try {
    config = createCapacityConfig();
    ({ report, exitCode } = await runCapacitySuite(config));
  } catch (error) {
    report = configurationFailureReport(error);
    exitCode = 2;
  }
  const serialized = `${JSON.stringify(report, null, 2)}\n`;
  const reportFile = config?.reportFile || process.env.SGLANG_CAPACITY_REPORT_FILE?.trim();
  if (reportFile) writeFileSync(reportFile, serialized, { mode: 0o600 });
  process.stdout.write(serialized);
  process.exitCode = exitCode;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  await main();
}
