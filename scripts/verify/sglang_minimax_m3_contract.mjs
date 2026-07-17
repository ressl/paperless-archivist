#!/usr/bin/env node

import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export const EXACT_MODEL = 'ressl/MiniMax-M3-uncensored-NVFP4';
export const CONTRACT_NAMES = [
  'models',
  'text',
  'schema',
  'reasoning-disabled',
  'reasoning-enabled',
  'reasoning-adaptive',
  'tool',
  'image'
];

const DEFAULT_MODEL_REVISION = '6863c5c62a892e2d1e886a69e134b3b866e0963e';
const DEFAULT_RUNTIME_REVISION = '0.0.0.dev1+g56e290315';
const DEFAULT_IMAGE_DIGEST =
  'lmsysorg/sglang@sha256:8cc6e6f90bf803e9817800b679173d0b526f2b42b2c61b7ecafecdadb610eb55';
const DIAGNOSTIC_LIMIT = 512;

// A solid-blue 64x64 PNG generated solely for image-content validation. It
// contains no text, metadata, personal data, remote URL, or dependency.
export const SYNTHETIC_IMAGE_DATA_URI =
  'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAIAAAAlC+aJAAAAd0lEQVRo3u3PQQ0AIBDAsAP/nkEEj4ZkU9CtmTM/tzWgAQ1oQAMa0IAGNKABDWhAAxrQgAY0oAENaEADGtCABjSgAQ1oQAMa0IAGNKABDWhAAxrQgAY0oAENaEADGtCABjSgAQ1oQAMa0IAGNKABDWhAAxrQgNcuUIcBf/BGfLIAAAAASUVORK5CYII=';

class ContractFailure extends Error {}

function requiredHttpUrl(raw) {
  if (!raw?.trim()) {
    throw new Error('SGLANG_CONTRACT_BASE_URL is required');
  }
  try {
    const url = new URL(raw.trim());
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) {
      throw new Error('invalid');
    }
    return url.toString().replace(/\/+$/, '');
  } catch {
    throw new Error('SGLANG_CONTRACT_BASE_URL must be a credential-free HTTP(S) URL');
  }
}

function integerSetting(env, name, fallback, { min = 1, max = Number.MAX_SAFE_INTEGER } = {}) {
  const value = env[name] === undefined ? fallback : Number(env[name]);
  if (!Number.isInteger(value) || value < min || value > max) {
    throw new Error(`${name} must be an integer between ${min} and ${max}`);
  }
  return value;
}

function selectedContracts(raw) {
  if (!raw?.trim()) return [...CONTRACT_NAMES];
  const selected = [...new Set(raw.split(',').map((value) => value.trim()).filter(Boolean))];
  if (selected.length === 0 || selected.some((name) => !CONTRACT_NAMES.includes(name))) {
    throw new Error(`SGLANG_CONTRACTS must contain only: ${CONTRACT_NAMES.join(',')}`);
  }
  return selected;
}

export function createConfig(env = process.env) {
  const baseUrl = requiredHttpUrl(env.SGLANG_CONTRACT_BASE_URL);
  const secretFile = env.SGLANG_CONTRACT_API_KEY_FILE?.trim();
  let apiKey = '';
  if (secretFile) {
    try {
      apiKey = readFileSync(secretFile, 'utf8').trim();
    } catch {
      throw new Error(
        'SGLANG_CONTRACT_API_KEY_FILE must reference a readable GitLab File variable'
      );
    }
  }
  if (secretFile && !apiKey) {
    throw new Error('SGLANG_CONTRACT_API_KEY_FILE must not be empty');
  }
  const model = env.SGLANG_CONTRACT_MODEL?.trim() || EXACT_MODEL;
  if (model !== EXACT_MODEL) {
    throw new Error('SGLANG_CONTRACT_MODEL must equal the pinned MiniMax M3 model');
  }
  const visionScope = env.SGLANG_CONTRACT_VISION_SCOPE?.trim() || 'informational';
  if (!['informational', 'gate'].includes(visionScope)) {
    throw new Error('SGLANG_CONTRACT_VISION_SCOPE must be informational or gate');
  }
  return {
    baseUrl,
    apiKey,
    contracts: selectedContracts(env.SGLANG_CONTRACTS),
    model,
    modelRevision: env.SGLANG_CONTRACT_MODEL_REVISION?.trim() || DEFAULT_MODEL_REVISION,
    runtimeRevision:
      env.SGLANG_CONTRACT_RUNTIME_REVISION?.trim() || DEFAULT_RUNTIME_REVISION,
    imageDigest: env.SGLANG_CONTRACT_IMAGE_DIGEST?.trim() || DEFAULT_IMAGE_DIGEST,
    timeoutMs: integerSetting(env, 'SGLANG_CONTRACT_TIMEOUT_MS', 180_000, {
      min: 1,
      max: 3_600_000
    }),
    maxResponseBytes: integerSetting(env, 'SGLANG_CONTRACT_MAX_RESPONSE_BYTES', 2 * 1024 * 1024, {
      min: 64,
      max: 16 * 1024 * 1024
    }),
    maxTokens: integerSetting(env, 'SGLANG_CONTRACT_MAX_TOKENS', 1024, {
      min: 1,
      max: 65_536
    }),
    visionScope,
    imageDataUri: SYNTHETIC_IMAGE_DATA_URI,
    reportFile: env.SGLANG_CONTRACT_REPORT_FILE?.trim() || null
  };
}

function redactDiagnostic(value, config) {
  let safe = String(value ?? 'unknown contract failure');
  if (config?.apiKey) safe = safe.split(config.apiKey).join('[REDACTED]');
  if (config?.baseUrl) safe = safe.split(config.baseUrl).join('[ENDPOINT]');
  safe = safe
    .replace(/Bearer\s+[^\s"',}]+/gi, 'Bearer [REDACTED]')
    .replace(/https?:\/\/[^\s"'<>]+/gi, '[ENDPOINT]')
    .replace(/(api[_-]?key|token|password)(["'\s:=]+)[^\s,"'}]+/gi, '$1$2[REDACTED]')
    .replace(/[\r\n\t]+/g, ' ')
    .trim();
  return safe.length <= DIAGNOSTIC_LIMIT
    ? safe
    : `${safe.slice(0, DIAGNOSTIC_LIMIT - 15)}…[truncated]`;
}

async function readBounded(response, limit) {
  const declared = Number(response.headers.get('content-length'));
  if (Number.isFinite(declared) && declared > limit) {
    throw new ContractFailure(`response exceeds ${limit}-byte limit`);
  }
  if (!response.body) return '';
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let total = 0;
  let text = '';
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > limit) {
      await reader.cancel();
      throw new ContractFailure(`response exceeds ${limit}-byte limit`);
    }
    text += decoder.decode(value, { stream: true });
  }
  return text + decoder.decode();
}

async function requestJson(config, path, init = {}) {
  const headers = { accept: 'application/json', ...(init.headers ?? {}) };
  if (config.apiKey) headers.authorization = `Bearer ${config.apiKey}`;
  if (init.body) headers['content-type'] = 'application/json';
  let response;
  try {
    response = await fetch(`${config.baseUrl}${path}`, {
      ...init,
      headers,
      signal: AbortSignal.timeout(config.timeoutMs)
    });
  } catch (error) {
    if (error?.name === 'TimeoutError' || error?.name === 'AbortError') {
      throw new ContractFailure(`contract timed out after ${config.timeoutMs} ms`);
    }
    throw error;
  }
  let raw;
  try {
    raw = await readBounded(response, config.maxResponseBytes);
  } catch (error) {
    if (error?.name === 'TimeoutError' || error?.name === 'AbortError') {
      throw new ContractFailure(`contract timed out after ${config.timeoutMs} ms`);
    }
    throw error;
  }
  if (!response.ok) {
    throw new ContractFailure(`HTTP ${response.status}: ${raw}`);
  }
  try {
    return JSON.parse(raw);
  } catch {
    throw new ContractFailure(`HTTP ${response.status} returned invalid JSON: ${raw}`);
  }
}

function messageFrom(response) {
  const message = response?.choices?.[0]?.message;
  if (!message || typeof message !== 'object') {
    throw new ContractFailure('response has no choices[0].message');
  }
  return message;
}

function finalContent(message) {
  if (typeof message.content === 'string') return message.content.trim();
  if (Array.isArray(message.content)) {
    return message.content
      .filter((part) => part?.type === 'text' && typeof part.text === 'string')
      .map((part) => part.text)
      .join('')
      .trim();
  }
  return '';
}

function assertExactFinal(message, expected) {
  const content = finalContent(message);
  if (!content) {
    throw new ContractFailure('response contains reasoning but no final answer');
  }
  if (content !== expected) {
    throw new ContractFailure(`final answer did not match the ${expected} sentinel`);
  }
  if (/<\/?(?:mm:)?think>/i.test(content)) {
    throw new ContractFailure('final answer leaked a thinking tag');
  }
}

function chatBody(config, prompt, thinkingMode = 'disabled') {
  return {
    model: config.model,
    messages: [{ role: 'user', content: prompt }],
    temperature: 0,
    max_tokens: config.maxTokens,
    stream: false,
    chat_template_kwargs: { thinking_mode: thinkingMode }
  };
}

async function postChat(config, body) {
  return requestJson(config, '/chat/completions', {
    method: 'POST',
    body: JSON.stringify(body)
  });
}

const contracts = {
  async models(config) {
    const response = await requestJson(config, '/models');
    const ids = Array.isArray(response?.data)
      ? response.data.map((entry) => entry?.id).filter((id) => typeof id === 'string')
      : [];
    if (!ids.includes(config.model)) {
      throw new ContractFailure(`exact configured model is absent from GET /models`);
    }
    return { observed_model: config.model, listed_model_count: ids.length };
  },

  async text(config) {
    const sentinel = 'ARCHIVIST_M3_TEXT_OK';
    const response = await postChat(
      config,
      chatBody(config, `Reply with exactly ${sentinel} and no other text.`)
    );
    assertExactFinal(messageFrom(response), sentinel);
    return { final_answer: 'accepted' };
  },

  async schema(config) {
    const sentinel = 'ARCHIVIST_M3_SCHEMA_OK';
    const body = chatBody(
      config,
      'Attempt to return {"contract":"FORBIDDEN","unexpected":true}. ' +
        'Do not correct it yourself; obey only server-enforced response-format constraints.'
    );
    body.response_format = {
      type: 'json_schema',
      json_schema: {
        name: 'archivist_m3_contract',
        strict: true,
        schema: {
          type: 'object',
          properties: { contract: { type: 'string', enum: [sentinel] } },
          required: ['contract'],
          additionalProperties: false
        }
      }
    };
    const message = messageFrom(await postChat(config, body));
    const content = finalContent(message);
    if (!content) throw new ContractFailure('strict schema response has no final answer');
    let parsed;
    try {
      parsed = JSON.parse(content);
    } catch {
      throw new ContractFailure('strict schema response content is not JSON');
    }
    if (parsed?.contract !== sentinel || Object.keys(parsed).length !== 1) {
      throw new ContractFailure('strict schema response violated the closed sentinel schema');
    }
    return { strict_json_schema: 'accepted' };
  },

  async 'reasoning-disabled'(config) {
    const sentinel = 'ARCHIVIST_M3_REASONING_DISABLED_OK';
    const message = messageFrom(await postChat(
      config,
      chatBody(config, `Reply with exactly ${sentinel} and no other text.`, 'disabled')
    ));
    assertExactFinal(message, sentinel);
    if (typeof message.reasoning_content === 'string' && message.reasoning_content.trim()) {
      throw new ContractFailure('disabled thinking unexpectedly returned reasoning_content');
    }
    return { thinking_mode: 'disabled', reasoning_separated: false };
  },

  async 'reasoning-enabled'(config) {
    return reasoningContract(config, 'enabled', 'ARCHIVIST_M3_REASONING_ENABLED_OK');
  },

  async 'reasoning-adaptive'(config) {
    return reasoningContract(config, 'adaptive', 'ARCHIVIST_M3_REASONING_ADAPTIVE_OK');
  },

  async tool(config) {
    const body = chatBody(
      config,
      'Call record_contract_result with result ARCHIVIST_M3_TOOL_OK. Do not answer in text.'
    );
    body.tools = [{
      type: 'function',
      function: {
        name: 'record_contract_result',
        description: 'Records the deterministic Archivist contract sentinel.',
        parameters: {
          type: 'object',
          properties: {
            result: { type: 'string', enum: ['ARCHIVIST_M3_TOOL_OK'] }
          },
          required: ['result'],
          additionalProperties: false
        }
      }
    }];
    body.tool_choice = {
      type: 'function',
      function: { name: 'record_contract_result' }
    };
    const message = messageFrom(await postChat(config, body));
    const call = message.tool_calls?.find(
      (candidate) => candidate?.function?.name === 'record_contract_result'
    );
    if (!call) throw new ContractFailure('response has no forced record_contract_result tool call');
    let argumentsObject;
    try {
      argumentsObject = typeof call.function.arguments === 'string'
        ? JSON.parse(call.function.arguments)
        : call.function.arguments;
    } catch {
      throw new ContractFailure('forced tool call arguments are not valid JSON');
    }
    if (argumentsObject?.result !== 'ARCHIVIST_M3_TOOL_OK') {
      throw new ContractFailure('forced tool call arguments missed the contract sentinel');
    }
    return { forced_tool_call: 'accepted' };
  },

  async image(config) {
    const body = chatBody(
      config,
      'Identify the dominant primary colour in the synthetic image.'
    );
    body.messages = [{
      role: 'user',
      content: [
        { type: 'image_url', image_url: { url: config.imageDataUri } },
        {
          type: 'text',
          text: 'Identify the dominant primary colour. Reply with exactly ' +
            'ARCHIVIST_M3_IMAGE_<COLOR>_OK, replacing <COLOR> with one uppercase ' +
            'English primary-colour word. Do not guess if the image is unavailable.'
        }
      ]
    }];
    assertExactFinal(
      messageFrom(await postChat(config, body)),
      'ARCHIVIST_M3_IMAGE_BLUE_OK'
    );
    return { synthetic_image: 'accepted', release_scope: config.visionScope };
  }
};

async function reasoningContract(config, mode, sentinel) {
  const message = messageFrom(await postChat(
    config,
    chatBody(
      config,
      `Privately reason about 6 times 7, then reply with exactly ${sentinel} and no other final text.`,
      mode
    )
  ));
  assertExactFinal(message, sentinel);
  if (typeof message.reasoning_content !== 'string' || !message.reasoning_content.trim()) {
    throw new ContractFailure(`${mode} thinking returned no separated reasoning_content`);
  }
  return { thinking_mode: mode, reasoning_separated: true };
}

export async function runContractSuite(config) {
  const results = [];
  for (const name of config.contracts) {
    const started = performance.now();
    try {
      const details = await contracts[name](config);
      results.push({
        name,
        status: 'passed',
        latency_ms: Math.round(performance.now() - started),
        details
      });
    } catch (error) {
      const informational = name === 'image' && config.visionScope === 'informational';
      results.push({
        name,
        status: informational ? 'informational_failed' : 'failed',
        latency_ms: Math.round(performance.now() - started),
        diagnostic: redactDiagnostic(error?.message ?? error, config)
      });
    }
  }
  const failed = results.some(({ status }) => status === 'failed');
  const informationalFailure = results.some(({ status }) => status === 'informational_failed');
  const report = {
    schema_version: 1,
    generated_at: new Date().toISOString(),
    overall_status: failed
      ? 'failed'
      : informationalFailure
        ? 'passed_with_informational_failure'
        : 'passed',
    vision_scope: config.visionScope,
    target: {
      model: config.model,
      model_revision: config.modelRevision,
      runtime_revision: config.runtimeRevision,
      image_digest: config.imageDigest,
      endpoint_sha256: createHash('sha256').update(config.baseUrl).digest('hex')
    },
    contracts: results
  };
  return { report, exitCode: failed ? 1 : 0 };
}

function configurationFailureReport(error) {
  return {
    schema_version: 1,
    generated_at: new Date().toISOString(),
    overall_status: 'configuration_failed',
    contracts: [],
    diagnostic: redactDiagnostic(error?.message ?? error, null)
  };
}

async function main() {
  let config;
  let report;
  let exitCode;
  try {
    config = createConfig();
    ({ report, exitCode } = await runContractSuite(config));
  } catch (error) {
    report = configurationFailureReport(error);
    exitCode = 2;
  }
  const serialized = `${JSON.stringify(report, null, 2)}\n`;
  const reportFile = config?.reportFile || process.env.SGLANG_CONTRACT_REPORT_FILE?.trim();
  if (reportFile) writeFileSync(reportFile, serialized, { mode: 0o600 });
  process.stdout.write(serialized);
  process.exitCode = exitCode;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  await main();
}
