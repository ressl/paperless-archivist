import assert from 'node:assert/strict';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  CONTRACT_NAMES,
  EXACT_MODEL,
  SYNTHETIC_IMAGE_DATA_URI,
  createConfig,
  runContractSuite
} from './sglang_minimax_m3_contract.mjs';

function json(response, status = 200) {
  return { status, body: JSON.stringify(response) };
}

function messageText(body) {
  const content = body.messages?.at(-1)?.content;
  if (typeof content === 'string') return content;
  return content?.find((part) => part.type === 'text')?.text ?? '';
}

function successfulResponse(body) {
  const prompt = messageText(body);
  if (body.tools) {
    return json({
      choices: [{
        message: {
          content: null,
          tool_calls: [{
            id: 'call_contract',
            type: 'function',
            function: {
              name: 'record_contract_result',
              arguments: JSON.stringify({ result: 'ARCHIVIST_M3_TOOL_OK' })
            }
          }]
        }
      }]
    });
  }
  if (body.response_format) {
    return json({
      choices: [{ message: { content: JSON.stringify({ contract: 'ARCHIVIST_M3_SCHEMA_OK' }) } }]
    });
  }
  if (Array.isArray(body.messages?.at(-1)?.content)) {
    return json({ choices: [{ message: { content: 'ARCHIVIST_M3_IMAGE_BLUE_OK' } }] });
  }
  if (prompt.includes('ARCHIVIST_M3_REASONING_DISABLED_OK')) {
    return json({ choices: [{ message: { content: 'ARCHIVIST_M3_REASONING_DISABLED_OK' } }] });
  }
  if (prompt.includes('ARCHIVIST_M3_REASONING_ENABLED_OK')) {
    return json({
      choices: [{ message: {
        content: 'ARCHIVIST_M3_REASONING_ENABLED_OK',
        reasoning_content: 'private chain of thought'
      } }]
    });
  }
  if (prompt.includes('ARCHIVIST_M3_REASONING_ADAPTIVE_OK')) {
    return json({
      choices: [{ message: {
        content: 'ARCHIVIST_M3_REASONING_ADAPTIVE_OK',
        reasoning_content: 'adaptive private chain of thought'
      } }]
    });
  }
  return json({ choices: [{ message: { content: 'ARCHIVIST_M3_TEXT_OK' } }] });
}

async function withMockServer(handler, callback) {
  const requests = [];
  const server = createServer(async (request, response) => {
    let raw = '';
    for await (const chunk of request) raw += chunk;
    const body = raw ? JSON.parse(raw) : null;
    requests.push({ method: request.method, url: request.url, headers: request.headers, body });
    const result = await handler({ request, body, requests });
    response.writeHead(result.status, { 'content-type': 'application/json' });
    response.end(result.body);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  try {
    return await callback(`http://127.0.0.1:${address.port}/v1`, requests);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
}

async function configFor(baseUrl, extra = {}) {
  const directory = await mkdtemp(join(tmpdir(), 'archivist-m3-contract-'));
  const secretFile = join(directory, 'api-key');
  await writeFile(secretFile, 'super-secret\n', { mode: 0o600 });
  const config = createConfig({
    SGLANG_CONTRACT_BASE_URL: baseUrl,
    SGLANG_CONTRACT_API_KEY_FILE: secretFile,
    ...extra
  });
  return { config, cleanup: () => rm(directory, { recursive: true, force: true }) };
}

test('all live contracts pass independently and the report contains no endpoint or secret', async () => {
  await withMockServer(async ({ request, body }) => {
    assert.equal(request.headers.authorization, 'Bearer super-secret');
    if (request.method === 'GET') {
      return json({ data: [{ id: EXACT_MODEL, owned_by: 'mock-owner' }] });
    }
    return successfulResponse(body);
  }, async (baseUrl, requests) => {
    const { config, cleanup } = await configFor(baseUrl);
    try {
      const { report, exitCode } = await runContractSuite(config);
      assert.equal(exitCode, 0);
      assert.deepEqual(report.contracts.map(({ name }) => name), CONTRACT_NAMES);
      assert.ok(report.contracts.every(({ status }) => status === 'passed'));
      assert.equal(report.overall_status, 'passed');
      assert.equal(report.target.model, EXACT_MODEL);
      assert.match(report.target.endpoint_sha256, /^[a-f0-9]{64}$/);
      assert.equal(requests.length, CONTRACT_NAMES.length);
      const serialized = JSON.stringify(report);
      assert.doesNotMatch(serialized, /super-secret/);
      assert.ok(!serialized.includes(baseUrl));
    } finally {
      await cleanup();
    }
  });
});

test('contract selection runs only the requested models and schema probes', async () => {
  await withMockServer(async ({ request, body }) =>
    request.method === 'GET'
      ? json({ data: [{ id: EXACT_MODEL }] })
      : successfulResponse(body),
  async (baseUrl, requests) => {
    const { config, cleanup } = await configFor(baseUrl, {
      SGLANG_CONTRACTS: 'models,schema'
    });
    try {
      const { report, exitCode } = await runContractSuite(config);
      assert.equal(exitCode, 0);
      assert.deepEqual(report.contracts.map(({ name }) => name), ['models', 'schema']);
      assert.equal(requests.length, 2);
    } finally {
      await cleanup();
    }
  });
});

test('configuration rejects a plain API key variable without echoing its value', () => {
  const exposedValue = 'misconfigured-plain-api-key-value';
  assert.throws(
    () => createConfig({
      SGLANG_CONTRACT_BASE_URL: 'https://sglang.example.invalid/v1',
      SGLANG_CONTRACT_API_KEY_FILE: exposedValue
    }),
    (error) => {
      assert.match(error.message, /readable.*file variable/i);
      assert.doesNotMatch(error.message, new RegExp(exposedValue));
      return true;
    }
  );
});

test('configuration cannot replace the exact MiniMax M3 identity', () => {
  assert.throws(
    () => createConfig({
      SGLANG_CONTRACT_BASE_URL: 'https://sglang.example.invalid/v1',
      SGLANG_CONTRACT_MODEL: 'other/model'
    }),
    /must equal the pinned MiniMax M3 model/i
  );
});

test('wrong model and schema 400 fail with bounded redacted diagnostics', async () => {
  await withMockServer(async ({ request, body }) => {
    if (request.method === 'GET') return json({ data: [{ id: 'wrong/model' }] });
    if (body.response_format) {
      return {
        status: 400,
        body: JSON.stringify({
          error: `Bearer super-secret schema rejected at http://private.internal/${'x'.repeat(900)}`
        })
      };
    }
    return successfulResponse(body);
  }, async (baseUrl) => {
    const { config, cleanup } = await configFor(baseUrl, {
      SGLANG_CONTRACTS: 'models,schema'
    });
    try {
      const { report, exitCode } = await runContractSuite(config);
      assert.equal(exitCode, 1);
      assert.deepEqual(report.contracts.map(({ status }) => status), ['failed', 'failed']);
      for (const contract of report.contracts) {
        assert.ok(contract.diagnostic.length <= 512);
        assert.doesNotMatch(contract.diagnostic, /super-secret|private\.internal/);
      }
    } finally {
      await cleanup();
    }
  });
});

test('schema fails when the server ignores the strict response format', async () => {
  await withMockServer(async () => json({
    choices: [{
      message: {
        content: JSON.stringify({ contract: 'FORBIDDEN', unexpected: true })
      }
    }]
  }), async (baseUrl, requests) => {
    const { config, cleanup } = await configFor(baseUrl, {
      SGLANG_CONTRACTS: 'schema'
    });
    try {
      const { report, exitCode } = await runContractSuite(config);
      assert.equal(exitCode, 1);
      assert.equal(report.contracts[0].status, 'failed');
      assert.doesNotMatch(messageText(requests[0].body), /ARCHIVIST_M3_SCHEMA_OK/);
    } finally {
      await cleanup();
    }
  });
});

test('reasoning-only output and missing forced tool call are contract failures', async () => {
  await withMockServer(async ({ body }) => {
    if (body.tools) return json({ choices: [{ message: { content: 'no tool call' } }] });
    return json({
      choices: [{ message: { content: null, reasoning_content: 'reasoning without final answer' } }]
    });
  }, async (baseUrl) => {
    const { config, cleanup } = await configFor(baseUrl, {
      SGLANG_CONTRACTS: 'reasoning-enabled,tool'
    });
    try {
      const { report, exitCode } = await runContractSuite(config);
      assert.equal(exitCode, 1);
      assert.match(report.contracts[0].diagnostic, /final answer/i);
      assert.match(report.contracts[1].diagnostic, /tool call/i);
    } finally {
      await cleanup();
    }
  });
});

test('invalid image is informational by default and fails an explicit vision gate', async () => {
  await withMockServer(async ({ body }) => {
    const imageUrl = body.messages[0].content.find((part) => part.type === 'image_url').image_url.url;
    assert.equal(imageUrl, 'data:image/png;base64,invalid');
    return { status: 400, body: JSON.stringify({ error: 'invalid image bytes' }) };
  }, async (baseUrl) => {
    const base = await configFor(baseUrl, { SGLANG_CONTRACTS: 'image' });
    try {
      const informational = await runContractSuite({
        ...base.config,
        imageDataUri: 'data:image/png;base64,invalid'
      });
      assert.equal(informational.exitCode, 0);
      assert.equal(informational.report.overall_status, 'passed_with_informational_failure');
      assert.equal(informational.report.contracts[0].status, 'informational_failed');

      const gating = await runContractSuite({
        ...base.config,
        imageDataUri: 'data:image/png;base64,invalid',
        visionScope: 'gate'
      });
      assert.equal(gating.exitCode, 1);
      assert.equal(gating.report.contracts[0].status, 'failed');
    } finally {
      await base.cleanup();
    }
  });
});

test('image fails when a provider ignores the image content', async () => {
  await withMockServer(async () => json({
    choices: [{ message: { content: 'ARCHIVIST_M3_IMAGE_UNKNOWN_OK' } }]
  }), async (baseUrl, requests) => {
    const base = await configFor(baseUrl, {
      SGLANG_CONTRACTS: 'image',
      SGLANG_CONTRACT_VISION_SCOPE: 'gate'
    });
    try {
      const { report, exitCode } = await runContractSuite(base.config);
      assert.equal(exitCode, 1);
      assert.equal(report.contracts[0].status, 'failed');
      const prompt = messageText(requests[0].body);
      assert.match(prompt, /dominant primary colo(?:u)?r/i);
      assert.doesNotMatch(prompt, /ARCHIVIST_M3_IMAGE_BLUE_OK/);
    } finally {
      await base.cleanup();
    }
  });
});

test('synthetic image is a small embedded PNG and contains no personal data', () => {
  assert.match(SYNTHETIC_IMAGE_DATA_URI, /^data:image\/png;base64,[A-Za-z0-9+/=]+$/);
  assert.ok(SYNTHETIC_IMAGE_DATA_URI.length < 1024);
  assert.doesNotMatch(SYNTHETIC_IMAGE_DATA_URI, /name|email|address/i);
  const png = Buffer.from(SYNTHETIC_IMAGE_DATA_URI.split(',')[1], 'base64');
  assert.deepEqual([...png.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10]);
  assert.equal(png.readUInt32BE(16), 64);
  assert.equal(png.readUInt32BE(20), 64);
});

test('oversized and timed-out responses fail without unbounded output', async () => {
  await withMockServer(async () => ({
    status: 200,
    body: JSON.stringify({ choices: [{ message: { content: 'x'.repeat(4096) } }] })
  }), async (baseUrl) => {
    const { config, cleanup } = await configFor(baseUrl, {
      SGLANG_CONTRACTS: 'text',
      SGLANG_CONTRACT_MAX_RESPONSE_BYTES: '512'
    });
    try {
      const { report, exitCode } = await runContractSuite(config);
      assert.equal(exitCode, 1);
      assert.match(report.contracts[0].diagnostic, /response.*limit/i);
      assert.ok(report.contracts[0].diagnostic.length <= 512);
    } finally {
      await cleanup();
    }
  });

  await withMockServer(async () => {
    await new Promise((resolve) => setTimeout(resolve, 100));
    return successfulResponse({ messages: [{ content: 'ARCHIVIST_M3_TEXT_OK' }] });
  }, async (baseUrl) => {
    const { config, cleanup } = await configFor(baseUrl, {
      SGLANG_CONTRACTS: 'text',
      SGLANG_CONTRACT_TIMEOUT_MS: '20'
    });
    try {
      const { report, exitCode } = await runContractSuite(config);
      assert.equal(exitCode, 1);
      assert.match(report.contracts[0].diagnostic, /timed out/i);
    } finally {
      await cleanup();
    }
  });
});
