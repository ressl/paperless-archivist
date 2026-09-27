import type { components } from './schema';

export type MetadataTrace = components['schemas']['MetadataTrace'];
export type MetadataTraceRun = components['schemas']['MetadataTraceRun'];
export type MetadataFieldOutcome = components['schemas']['MetadataFieldOutcome'];
export type AiRuntimeHints = components['schemas']['AiRuntimeHints'];
export type AiLoadedModel = components['schemas']['AiLoadedModel'];

export type ReasoningEffort = components['schemas']['ReasoningEffort'];
export type StructuredOutputMode = components['schemas']['StructuredOutputMode'];
export type ModelCapability = components['schemas']['ModelCapability'];
export type ModelUsageTier = components['schemas']['ModelUsageTier'];
export type ModelCatalogEntry = components['schemas']['ModelCatalogEntry'];
export type ProviderTuning = components['schemas']['ProviderTuning'];
export type ProviderTestRequest = components['schemas']['ProviderTestRequest'];
export type ProviderTestResponse = components['schemas']['ProviderTestResponse'];
export type Role = components['schemas']['Role'];
export type PipelineStage = components['schemas']['Stage'];
export type Stage = Exclude<PipelineStage, 'apply'>;
export type ProcessingMode = components['schemas']['ProcessingMode'];
export type AiProviderKind = components['schemas']['AiProviderKind'];
export type AiProvider = components['schemas']['AiProviderSettings'];

export type OllamaInstalledModel = {
  name: string;
  parameter_size?: string | null;
  quantization_level?: string | null;
  size_bytes?: number | null;
  size_gb?: number | null;
  modified_at?: string | null;
  digest?: string | null;
};

export type PaperlessConsistencyResult = {
  ok: boolean;
  documents_checked: number;
  missing_local: number[];
  stale_local: number[];
  mismatches: Array<{ paperless_document_id: number; fields: string[] }>;
};

export type CompletionTagReconcileResult = {
  dry_run: boolean;
  planned: Array<{ paperless_document_id: number; add: string[] }>;
  applied: number[];
};

export type RuntimeSettings = components['schemas']['RuntimeSettings'];
export type RuntimeSettingsInput = components['schemas']['RuntimeSettingsInput'];

export type Permissions = {
  read_dashboard: boolean;
  read_runs: boolean;
  write_runs: boolean;
  read_inventory: boolean;
  write_batches: boolean;
  use_chat: boolean;
  read_reviews: boolean;
  write_reviews: boolean;
  read_settings: boolean;
  write_settings: boolean;
  manage_users: boolean;
  read_audit: boolean;
};

export type Me = {
  username: string;
  roles: Role[];
  permissions: Permissions;
  csrf_token?: string | null;
};

export type OidcConfig = {
  enabled: boolean;
  login_url?: string | null;
  provider?: string | null;
  paperless_login_enabled: boolean;
};

export type Counts = {
  total_documents: number;
  complete: number;
  missing_ocr: number;
  waiting_review: number;
  failed: number;
  running: number;
  never_processed: number;
};

export type DashboardRange = '24h' | '7d' | '30d' | '90d' | '12m' | 'all';

export type DashboardRangeOption = {
  key: DashboardRange;
  label: string;
};

export type DashboardKpis = {
  completion_rate: number;
  open_backlog: number;
  failure_rate: number;
  review_load: number;
  running_jobs: number;
  throughput: number;
  cost_in_range_usd?: number | null;
  mttc_seconds?: number | null;
  p95_stage_duration_ms?: number | null;
};

export type DashboardCostBucket = {
  bucket: string;
  label: string;
  cost_usd?: number | null;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
};

export type DashboardProviderCostSummary = {
  provider: string;
  model: string;
  cost_usd?: number | null;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  sparkline: Array<number | null>;
};

export type NeedsAttentionItem = {
  kind: string;
  severity: 'info' | 'warning' | 'critical' | string;
  title: string;
  description: string;
  action_key?: string | null;
  count?: number | null;
};

export type DashboardComparison = {
  jobs_created_delta: number;
  jobs_succeeded_delta: number;
  jobs_failed_delta: number;
  open_backlog_delta: number;
};

export type DashboardStageStatus = {
  stage: string;
  complete: number;
  pending: number;
  failed: number;
  waiting_review: number;
  running: number;
};

export type DashboardTimeBucket = {
  bucket: string;
  label: string;
  jobs_created: number;
  jobs_succeeded: number;
  jobs_failed: number;
  runs_created: number;
  runs_succeeded: number;
  runs_failed: number;
};

export type DashboardBacklogPoint = {
  bucket: string;
  label: string;
  total_documents: number;
  complete: number;
  open_backlog: number;
  failed: number;
  waiting_review: number;
  running: number;
};

export type DashboardStatusCount = {
  status: string;
  count: number;
};

export type ProviderUsageStats = {
  provider: string;
  model: string;
  stage: string;
  request_count: number;
  avg_duration_ms: number;
  p95_duration_ms: number;
  input_tokens: number;
  output_tokens: number;
  estimated_cost_usd?: number | null;
  feedback_count: number;
  positive_feedback: number;
  negative_feedback: number;
  acceptance_rate?: number | null;
  latency_history: Array<number | null>;
};

export type QualityStats = {
  review_decisions: number;
  review_approved: number;
  review_edited: number;
  review_rejected: number;
  acceptance_rate?: number | null;
  uncertainty_reviews: number;
  validation_warning_reviews: number;
};

export type DashboardStats = {
  generated_at: string;
  selected_range: DashboardRange;
  available_ranges: DashboardRangeOption[];
  kpis: DashboardKpis;
  comparison: DashboardComparison;
  stage_status: DashboardStageStatus[];
  throughput_series: DashboardTimeBucket[];
  backlog_series: DashboardBacklogPoint[];
  job_status: DashboardStatusCount[];
  run_status: DashboardStatusCount[];
  review_status: DashboardStatusCount[];
  provider_usage: ProviderUsageStats[];
  quality: QualityStats;
  cost_series: DashboardCostBucket[];
  cost_breakdown_by_provider: DashboardProviderCostSummary[];
};

/** Month-to-date AI cost against the configured monthly budget (#450). */
export type CostBudgetStatus = components['schemas']['CostBudgetStatus'];

export type DashboardResponse = {
  counts: Counts;
  stats: DashboardStats;
  /** Null (or absent from older servers) when no budget is configured. */
  budget?: CostBudgetStatus | null;
};

export type ServiceProcessingStatus = {
  state: 'idle' | 'running' | 'error' | string;
  title: string;
  description: string;
  last_event_at?: string | null;
};

export type WorkflowSafetyStatus = {
  paused: boolean;
  dry_run: boolean;
  hourly_document_limit?: number | null;
  daily_document_limit?: number | null;
  hourly_remaining?: number | null;
  daily_remaining?: number | null;
};

export type DashboardLiveRun = {
  id: string;
  trace_id: string;
  paperless_document_id: number;
  mode: ProcessingMode;
  status: string;
  trigger_tag: string;
  stages: PipelineStage[];
  started_at?: string | null;
  created_at: string;
  updated_at: string;
};

export type DashboardLiveJob = {
  id: string;
  run_id: string;
  trace_id: string;
  paperless_document_id: number;
  stage: PipelineStage;
  status: string;
  attempts: number;
  max_attempts: number;
  lease_owner?: string | null;
  lease_until?: string | null;
  updated_at: string;
  error_message?: string | null;
};

export type DashboardLiveLlmEvent = {
  id: string;
  run_id: string;
  job_id?: string | null;
  stage: PipelineStage;
  provider: string;
  model: string;
  duration_ms?: number | null;
  created_at: string;
};

export type DashboardLiveFailure = {
  id: string;
  run_id: string;
  paperless_document_id: number;
  stage: PipelineStage;
  status: string;
  failure_kind: string;
  attempts: number;
  error_message: string;
  next_attempt_at?: string | null;
  updated_at: string;
};

export type DashboardLiveStatus = {
  generated_at: string;
  workflow_mode: ProcessingMode;
  autopilot_enabled: boolean;
  workflow_safety: WorkflowSafetyStatus;
  selector: ServiceProcessingStatus;
  next_selector_scan_at?: string | null;
  llm: ServiceProcessingStatus;
  paperless: ServiceProcessingStatus;
  active_runs: DashboardLiveRun[];
  active_jobs: DashboardLiveJob[];
  recent_llm_events: DashboardLiveLlmEvent[];
  recent_failures: DashboardLiveFailure[];
  needs_attention: NeedsAttentionItem[];
};

export type InventoryQueryParams = {
  limit?: number;
  offset?: number;
  id?: number;
  q?: string;
  ocr_status?: string[];
  metadata_status?: string[];
  run_status?: string[];
  tag?: string[];
  not_tag?: string[];
  lang?: string;
  date_from?: string;
  date_to?: string;
  has_error?: boolean;
  needs_review?: boolean;
};

export type InventoryItem = {
  paperless_document_id: number;
  title?: string | null;
  original_file_name?: string | null;
  current_tags: string[];
  ocr_status: string;
  /** Consolidated v1.4+ metadata stage status. */
  metadata_status: string;
  current_run_status?: string | null;
  last_error?: string | null;
  needs_review: boolean;
  complete: boolean;
  document_date?: string | null;
  detected_language?: string | null;
  detected_language_confidence?: number | null;
  detected_language_source?: string | null;
  debug_context?: WorkflowDebugContext | null;
};

export type DuplicateDocument = {
  paperless_document_id: number;
  title?: string | null;
};

export type DuplicateGroup = {
  hash: string;
  documents: DuplicateDocument[];
};

export type WorkflowDebugContext = {
  selector_reason?: string | null;
  workflow_mode?: ProcessingMode | string | null;
  workflow_paused?: boolean | null;
  dry_run?: boolean | null;
  prompt_language?: string | null;
  tag_output_language?: string | null;
  detected_language?: string | null;
  detected_language_confidence?: number | null;
  detected_language_source?: string | null;
  current_run_status?: string | null;
  last_error?: string | null;
  next_required_stage?: string | null;
};

export type ReviewItem = {
  id: string;
  paperless_document_id: number;
  stage: string;
  status: string;
  suggested_patch: unknown;
  edited_patch?: unknown;
  validation_warnings?: unknown;
  conflict_fields?: string[];
  conflicted_at?: string | null;
  debug_context?: WorkflowDebugContext | null;
  paperless_title?: string | null;
  created_at: string;
};

export type RecoveryCandidate = {
  run_id: string;
  job_id?: string | null;
  paperless_document_id: number;
  stage?: PipelineStage | null;
  status: string;
  lease_owner?: string | null;
  lease_until?: string | null;
  updated_at: string;
  reason: string;
};

export type RecoverySummary = {
  stale_leases_requeued: number;
  stuck_runs_failed: number;
  stuck_runs_completed: number;
};

export type ProviderCooldown = {
  provider_name: string;
  cooldown_until: string;
  reason: string;
  set_at: string;
};

export type DocumentChatSource = {
  paperless_document_id: number;
  title?: string | null;
  snippet: string;
  score: number;
  source_kind: string;
};

export type DocumentChatSession = {
  id: string;
  title: string;
  created_by?: string | null;
  created_at: string;
  updated_at: string;
};

export type DocumentChatMessage = {
  id: string;
  session_id: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  provider?: string | null;
  model?: string | null;
  metadata?: unknown;
  sources: DocumentChatSource[];
  created_at: string;
};

export type AuditEvent = {
  id: string;
  event_type: string;
  actor_type: string;
  actor_id?: string | null;
  paperless_document_id?: number | null;
  outcome: string;
  error_message?: string | null;
  created_at: string;
  metadata?: unknown;
  prev_event_hash?: string | null;
  event_hash?: string | null;
  hash_version?: number | null;
};

export type ApiToken = {
  id: string;
  name: string;
  scopes: string[];
  expires_at?: string | null;
  revoked_at?: string | null;
  last_used_at?: string | null;
  created_at: string;
};

export type AuditIntegrityReport = {
  ok: boolean;
  checked_events: number;
  legacy_events: number;
  v1_events: number;
  v2_events: number;
  legacy_precision_events: number;
  latest_event_hash?: string | null;
  broken_event_id?: string | null;
  broken_reason?: string | null;
};

export type RetentionResult = {
  audit_events_deleted: number;
  ai_artifacts_deleted: number;
  ocr_page_cache_deleted: number;
};

export type Prompt = {
  id: string;
  stage: Stage;
  name: string;
  version: number;
  content: string;
  output_schema?: unknown;
  active: boolean;
  created_at: string;
};

export type PromptUsage = {
  prompt_id: string;
  run_count: number;
  job_count: number;
  last_used_at?: string | null;
  avg_duration_ms: number;
  last_provider?: string | null;
  last_model?: string | null;
};

export type PromptExperiment = {
  group: string;
  total: number;
  approved: number;
  rejected: number;
  edited: number;
  applied: number;
  mean_confidence?: number | null;
};

export type PromptTestResponse = {
  provider: string;
  model: string;
  stage: Stage;
  raw_text: string;
  parsed: components['schemas']['PromptTestParsed'];
  validation_errors: string[];
  warnings: string[];
  duration_ms: number;
};

export type SessionItem = {
  id: string;
  user_id: string;
  username: string;
  expires_at: string;
  revoked_at?: string | null;
  last_seen_at?: string | null;
  created_at: string;
};

export type UserItem = {
  id: string;
  username: string;
  email?: string | null;
  roles: Role[];
  enabled: boolean;
  last_login_at?: string | null;
  created_at: string;
};

// --- Statistics page (GET /api/statistics) ---------------------------------
// Usage/cost analytics over a free time range, mirroring the backend
// StatisticsResponse contract. input_tokens is often 0 (Ollama input tokens
// are redacted upstream) and estimated_cost_usd is null unless the provider
// has cost configured.
export type StatisticsBucket = 'hour' | 'day' | 'week' | 'month';

export type StatisticsQueryParams = {
  from?: string;
  to?: string;
  bucket?: StatisticsBucket;
};

export type StatisticsSummary = {
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  avg_duration_ms: number;
  estimated_cost_usd?: number | null;
  jobs_succeeded: number;
  jobs_failed: number;
  jobs_cancelled: number;
};

export type StatisticsTimePoint = {
  bucket: string;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  avg_duration_ms: number;
};

export type StatisticsThroughputPoint = {
  bucket: string;
  succeeded: number;
  failed: number;
  cancelled: number;
};

export type StatisticsBreakdownRow = {
  provider?: string;
  model?: string;
  stage?: string;
  request_count: number;
  input_tokens: number;
  output_tokens: number;
  avg_duration_ms: number;
  estimated_cost_usd?: number | null;
};

export type StatisticsResponse = {
  from: string;
  to: string;
  bucket: StatisticsBucket;
  summary: StatisticsSummary;
  time_series: StatisticsTimePoint[];
  throughput_series: StatisticsThroughputPoint[];
  by_provider: StatisticsBreakdownRow[];
  by_model: StatisticsBreakdownRow[];
  by_stage: StatisticsBreakdownRow[];
};

function csrfToken(): string | undefined {
  const match = document.cookie
    .split(';')
    .map((part) => part.trim())
    .find((part) => part.startsWith('pa_csrf='));
  return match ? decodeURIComponent(match.slice('pa_csrf='.length)) : undefined;
}

// Invoked whenever any request gets a 401, so the app can drop back to the
// login screen instead of every poller (dashboard, debug console) re-raising
// "Unauthorized" into the error banner forever after the session expires.
let unauthorizedHandler: (() => void) | null = null;

export function setUnauthorizedHandler(handler: (() => void) | null): void {
  unauthorizedHandler = handler;
}

/**
 * Error thrown for every non-2xx API response (and for a 2xx body that is not
 * valid JSON). Keeps the backend's `{ "error": "..." }` text as `message`, so
 * callers that only read `err.message` behave exactly as before, but also
 * carries the HTTP `status` and an optional machine-readable `code` so callers
 * can branch on the error type instead of matching substrings. (#432)
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string | undefined;

  constructor(message: string, status: number, code?: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

/** `code` of the ApiError thrown when a 2xx response body is not valid JSON. */
export const INVALID_RESPONSE_CODE = 'invalid_response';

export function isApiError(err: unknown): err is ApiError {
  return err instanceof ApiError;
}

/** True when `err` is the rejection of a request cancelled via its AbortSignal. */
export function isAbortError(err: unknown): boolean {
  return typeof err === 'object' && err !== null && (err as { name?: unknown }).name === 'AbortError';
}

// Endpoints whose 401 means "these credentials are wrong", not "the session
// expired": a failed login attempt or a wrong current password must surface as
// a form error instead of bouncing the user to the login screen. Every other
// /api/auth/* call (me, sessions, revoke, logout) does signal an expired
// session. (#432)
const CREDENTIAL_CHECK_PATHS = new Set(['/api/auth/login', '/api/auth/paperless-login', '/api/auth/change-password']);

/** Optional per-call request options (currently only cancellation). */
export type RequestOptions = { signal?: AbortSignal };

function parseJson(text: string): { ok: true; value: unknown } | { ok: false } {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch {
    return { ok: false };
  }
}

function requestHeaders(init: RequestInit): Headers {
  const headers = new Headers(init.headers);
  if (init.body && !headers.has('content-type')) {
    headers.set('content-type', 'application/json');
  }
  const method = (init.method ?? 'GET').toUpperCase();
  const csrf = csrfToken();
  if (csrf && !['GET', 'HEAD', 'OPTIONS'].includes(method)) {
    headers.set('x-csrf-token', csrf);
  }
  return headers;
}

/** Build the ApiError for a non-2xx response (and signal an expired session). */
function responseError(path: string, response: Response, text: string): ApiError {
  let message = `${response.status} ${response.statusText}`.trim();
  let code: string | undefined;
  const parsed = text ? parseJson(text) : null;
  if (parsed?.ok && parsed.value && typeof parsed.value === 'object') {
    const body = parsed.value as { error?: unknown; code?: unknown };
    if (typeof body.error === 'string' && body.error) message = body.error;
    if (typeof body.code === 'string' && body.code) code = body.code;
  }
  // A 401 on any call but a credential check means the session expired;
  // notify the app so it returns to the login screen.
  if (response.status === 401 && !CREDENTIAL_CHECK_PATHS.has(path.split('?')[0])) {
    unauthorizedHandler?.();
  }
  return new ApiError(message, response.status, code);
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(path, {
    ...init,
    credentials: 'include',
    headers: requestHeaders(init)
  });
  const text = await response.text();
  if (!response.ok) {
    throw responseError(path, response, text);
  }
  // 204 No Content / empty body: nothing to parse.
  if (!text) return undefined as T;
  const parsed = parseJson(text);
  if (!parsed.ok) {
    // e.g. an HTML page from a misconfigured proxy: fail with a typed error
    // instead of leaking a bare SyntaxError from JSON.parse.
    throw new ApiError(`Unexpected non-JSON response (${response.status})`, response.status, INVALID_RESPONSE_CODE);
  }
  return parsed.value as T;
}

export type ChatMessageResult = {
  session_id: string;
  user_message_id: string;
  assistant_message_id: string;
  answer: string;
  sources: DocumentChatSource[];
};

export type ChatStreamHandlers = {
  onSources?: (sources: DocumentChatSource[]) => void;
  onDelta?: (text: string) => void;
};

/** `code` of the ApiError for a streamed answer that failed after it started (#449). */
export const STREAM_ERROR_CODE = 'stream_error';

type SseEvent = { event: string; data: string };

/**
 * Split complete server-sent-event frames off `buffer` (#449). Returns the
 * parsed events and the unconsumed tail (a frame still being received).
 * Comment lines (`: keep-alive`) and frames without data are skipped.
 */
export function parseSseFrames(buffer: string): { events: SseEvent[]; rest: string } {
  const normalized = buffer.replace(/\r\n?/g, '\n');
  const frames = normalized.split('\n\n');
  const rest = frames.pop() ?? '';
  const events: SseEvent[] = [];
  for (const frame of frames) {
    let event = 'message';
    const data: string[] = [];
    for (const line of frame.split('\n')) {
      if (!line || line.startsWith(':')) continue;
      const separator = line.indexOf(':');
      const field = separator === -1 ? line : line.slice(0, separator);
      let value = separator === -1 ? '' : line.slice(separator + 1);
      if (value.startsWith(' ')) value = value.slice(1);
      if (field === 'event') event = value;
      else if (field === 'data') data.push(value);
    }
    if (data.length > 0) events.push({ event, data: data.join('\n') });
  }
  return { events, rest };
}

/**
 * Ask a chat question and stream the answer from the API's same-origin SSE
 * endpoint (#449). `fetch` + a stream reader instead of `EventSource`, which
 * cannot POST or send the CSRF header. Resolves with the stored exchange once
 * the `done` event arrives; rejects with an ApiError for HTTP errors, an
 * `error` event or a stream that ends without `done`.
 */
async function streamChatMessage(
  id: string,
  input: { question: string; document_ids?: number[] | null; max_sources?: number },
  handlers: ChatStreamHandlers = {},
  options: RequestOptions = {}
): Promise<ChatMessageResult> {
  const path = `/api/chat/sessions/${encodeURIComponent(id)}/messages/stream`;
  const init: RequestInit = {
    method: 'POST',
    body: JSON.stringify(input),
    signal: options.signal,
    headers: { accept: 'text/event-stream' }
  };
  const response = await fetch(path, { ...init, credentials: 'include', headers: requestHeaders(init) });
  if (!response.ok) throw responseError(path, response, await response.text());
  if (!response.body) {
    throw new ApiError('Streaming responses are not supported by this browser', response.status, STREAM_ERROR_CODE);
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  try {
    for (;;) {
      const { value, done } = await reader.read();
      buffer += done ? decoder.decode() : decoder.decode(value, { stream: true });
      // At the end of the body a final frame may lack its blank line.
      const parsed = parseSseFrames(done ? `${buffer}\n\n` : buffer);
      buffer = parsed.rest;
      for (const event of parsed.events) {
        const payload = parseJson(event.data);
        if (!payload.ok || !payload.value || typeof payload.value !== 'object') continue;
        const body = payload.value as Record<string, unknown>;
        switch (event.event) {
          case 'sources':
            if (Array.isArray(body.sources)) handlers.onSources?.(body.sources as DocumentChatSource[]);
            break;
          case 'delta':
            if (typeof body.text === 'string' && body.text) handlers.onDelta?.(body.text);
            break;
          case 'done':
            return body as unknown as ChatMessageResult;
          case 'error':
            throw new ApiError(
              typeof body.error === 'string' && body.error ? body.error : 'Request failed',
              500,
              STREAM_ERROR_CODE
            );
        }
      }
      if (done) break;
    }
  } finally {
    // Release the connection when we stop early (done/error/abort).
    void reader.cancel().catch(() => undefined);
  }
  throw new ApiError('The answer stream ended unexpectedly', 502, STREAM_ERROR_CODE);
}

export const api = {
  login: (username: string, password: string) =>
    request<Me>('/api/auth/login', {
      method: 'POST',
      body: JSON.stringify({ username, password })
    }),
  paperlessLogin: (username: string, password: string) =>
    request<Me>('/api/auth/paperless-login', {
      method: 'POST',
      body: JSON.stringify({ username, password })
    }),
  oidcConfig: (options?: RequestOptions) => request<OidcConfig>('/api/auth/oidc/config', options),
  logout: () => request<{ ok: boolean }>('/api/auth/logout', { method: 'POST' }),
  me: (options?: RequestOptions) => request<Me>('/api/auth/me', options),
  settings: (options?: RequestOptions) => request<RuntimeSettings>('/api/settings', options),
  saveSettings: (settings: RuntimeSettingsInput, paperlessToken?: string, providerSecrets?: Record<string, string>, notificationWebhookUrl?: string) =>
    request<RuntimeSettings>('/api/settings', {
      method: 'PUT',
      body: JSON.stringify({
        settings,
        paperless_token: paperlessToken || null,
        provider_secrets: providerSecrets || null,
        notification_webhook_url: notificationWebhookUrl || null
      })
    }),
  testPaperless: () => request<{ ok: boolean; error?: string }>('/api/settings/test-paperless', { method: 'POST' }),
  testNotification: () => request<{ ok: boolean; error?: string }>('/api/notifications/test', { method: 'POST' }),
  testProvider: (input: ProviderTestRequest) =>
    request<ProviderTestResponse>('/api/model-providers/test', {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  ollamaModels: (providerName: string) =>
    request<{ provider: string; models: OllamaInstalledModel[] }>(`/api/model-providers/${encodeURIComponent(providerName)}/models`, { method: 'POST' }),
  aiRuntimeHints: (provider?: string) => {
    const query = provider ? `?provider=${encodeURIComponent(provider)}` : '';
    return request<AiRuntimeHints>(`/api/ai/runtime-hints${query}`);
  },
  syncPaperless: () => request<Record<string, unknown>>('/api/paperless/sync-metadata', { method: 'POST' }),
  paperlessConsistency: (options?: RequestOptions) => request<PaperlessConsistencyResult>('/api/paperless/consistency', options),
  reconcileCompletionTags: (input: { dry_run?: boolean; document_ids?: number[] }) =>
    request<CompletionTagReconcileResult>('/api/paperless/completion-tags/reconcile', {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  dashboard: (range: DashboardRange = '24h', options?: RequestOptions) => request<DashboardResponse>(`/api/dashboard?range=${encodeURIComponent(range)}`, options),
  statistics: (params: StatisticsQueryParams = {}, options?: RequestOptions) => {
    const qs = new URLSearchParams();
    if (params.from) qs.set('from', params.from);
    if (params.to) qs.set('to', params.to);
    if (params.bucket) qs.set('bucket', params.bucket);
    const query = qs.toString();
    return request<StatisticsResponse>(`/api/statistics${query ? `?${query}` : ''}`, options);
  },
  dashboardLive: (options?: RequestOptions) => request<DashboardLiveStatus>('/api/dashboard/live', options),
  updateWorkflowMode: (mode: ProcessingMode) =>
    request<RuntimeSettings>('/api/workflow/mode', {
      method: 'PUT',
      body: JSON.stringify({ mode })
    }),
  updateWorkflowControls: (patch: Partial<Pick<RuntimeSettings['workflow'], 'paused' | 'dry_run' | 'hourly_document_limit' | 'daily_document_limit'>>) =>
    request<RuntimeSettings>('/api/workflow/controls', {
      method: 'PATCH',
      body: JSON.stringify(patch)
    }),
  inventory: (params: InventoryQueryParams = {}, options?: RequestOptions) => {
    const qs = new URLSearchParams();
    qs.set('limit', String(params.limit ?? 500));
    qs.set('offset', String(params.offset ?? 0));
    if (params.id != null) qs.set('id', String(params.id));
    if (params.q) qs.set('q', params.q);
    if (params.ocr_status && params.ocr_status.length) qs.set('ocr_status', params.ocr_status.join(','));
    if (params.metadata_status && params.metadata_status.length) qs.set('metadata_status', params.metadata_status.join(','));
    if (params.run_status && params.run_status.length) qs.set('run_status', params.run_status.join(','));
    if (params.tag && params.tag.length) qs.set('tag', params.tag.join(','));
    if (params.not_tag && params.not_tag.length) qs.set('not_tag', params.not_tag.join(','));
    if (params.lang) qs.set('lang', params.lang);
    if (params.date_from) qs.set('date_from', params.date_from);
    if (params.date_to) qs.set('date_to', params.date_to);
    if (params.has_error != null) qs.set('has_error', String(params.has_error));
    if (params.needs_review != null) qs.set('needs_review', String(params.needs_review));
    return request<{ items: InventoryItem[]; total: number; offset: number; limit: number }>(
      `/api/inventory?${qs.toString()}`, options
    );
  },
  inventoryDuplicates: () =>
    request<{ groups: DuplicateGroup[]; paperless_base: string }>('/api/inventory/duplicates'),
  inventoryMetadataTrace: (documentId: number, options?: RequestOptions) =>
    request<MetadataTrace>(`/api/inventory/${documentId}/metadata-trace`, options),
  queueOcr: () => request<{ queued: number }>('/api/batches/ocr', { method: 'POST' }),
  queueFull: () => request<{ queued: number }>('/api/batches/full', { method: 'POST' }),
  bulkRerun: (document_ids: number[], stages: Stage[]) =>
    request<{ queued: number }>('/api/batches/rerun', {
      method: 'POST',
      body: JSON.stringify({ document_ids, stages })
    }),
  rerunFailed: () =>
    request<{ queued: number; candidates: number }>('/api/batches/rerun-failed', {
      method: 'POST'
    }),
  triggerDocument: (paperless_document_id: number, stages: Stage[], mode: ProcessingMode) =>
    request<{ run_id: string }>(`/api/documents/${paperless_document_id}/trigger`, {
      method: 'POST',
      body: JSON.stringify({ stages, mode })
    }),
  // `paperless_base`: browser-facing Paperless URL for source links (#449).
  chatSessions: (options?: RequestOptions) =>
    request<{ items: DocumentChatSession[]; paperless_base?: string }>('/api/chat/sessions', options),
  renameChatSession: (id: string, title: string) =>
    request<{ id: string; title: string }>(`/api/chat/sessions/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      body: JSON.stringify({ title })
    }),
  deleteChatSession: (id: string) =>
    request<{ id: string; deleted: boolean }>(`/api/chat/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' }),
  streamChatMessage,
  createChatSession: (title?: string) =>
    request<{ id: string; title: string }>('/api/chat/sessions', {
      method: 'POST',
      body: JSON.stringify({ title: title || null })
    }),
  chatMessages: (id: string, options?: RequestOptions) => request<{ items: DocumentChatMessage[] }>(`/api/chat/sessions/${id}`, options),
  postChatMessage: (id: string, input: { question: string; document_ids?: number[] | null; max_sources?: number }) =>
    request<{
      session_id: string;
      user_message_id: string;
      assistant_message_id: string;
      answer: string;
      sources: DocumentChatSource[];
    }>(`/api/chat/sessions/${id}/messages`, {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  reviews: (limit = 100, options?: RequestOptions) =>
    request<{ items: ReviewItem[]; total: number; has_more: boolean }>(
      `/api/reviews?status=pending&limit=${encodeURIComponent(String(limit))}`, options
    ),
  approveReview: (id: string) => request<{ ok: boolean }>(`/api/reviews/${id}/approve`, { method: 'POST' }),
  rejectReview: (id: string) => request<{ ok: boolean }>(`/api/reviews/${id}/reject`, { method: 'POST' }),
  autoFixReviewPreview: (limit?: number) =>
    request<{ total_pending: number; would_apply: number; would_reject: number; sample: unknown[] }>(
      '/api/reviews/auto-fix-preview',
      { method: 'POST', body: JSON.stringify({ limit }) }
    ),
  autoFixReviewBulk: (limit?: number) =>
    request<{ applied: number; rejected: number; errors: unknown[] }>('/api/reviews/auto-fix', {
      method: 'POST',
      body: JSON.stringify({ limit }),
    }),
  autoFixReviewSingle: (id: string) =>
    request<{ action: 'applied' | 'rejected' }>(`/api/reviews/${id}/auto-fix`, { method: 'POST' }),
  batchReview: (ids: string[], decision: 'approve' | 'reject') =>
    request<{ ok: boolean; succeeded: string[]; failed: Array<{ id: string; error: string }> }>('/api/reviews/batch', {
      method: 'POST',
      body: JSON.stringify({ ids, decision })
    }),
  editReview: (id: string, patch: unknown) =>
    request<{ ok: boolean }>(`/api/reviews/${id}/edit`, {
      method: 'POST',
      body: JSON.stringify({ patch })
    }),
  recoveryStatus: (olderThanSeconds = 600) =>
    request<{ older_than_seconds: number; items: RecoveryCandidate[] }>(
      `/api/operations/recovery?older_than_seconds=${encodeURIComponent(String(olderThanSeconds))}`
    ),
  recoverStaleLeases: (olderThanSeconds = 600) =>
    request<{ older_than_seconds: number; summary: RecoverySummary }>('/api/operations/recovery/stale-leases', {
      method: 'POST',
      body: JSON.stringify({ older_than_seconds: olderThanSeconds })
    }),
  recoverStuckRuns: (olderThanSeconds = 600) =>
    request<{ older_than_seconds: number; summary: RecoverySummary }>('/api/operations/recovery/stuck-runs', {
      method: 'POST',
      body: JSON.stringify({ older_than_seconds: olderThanSeconds })
    }),
  unblockJobs: (input: { error_substring?: string | null; clear_provider_cooldowns?: boolean } = {}) =>
    request<{ predecessors_requeued: number; runs_unblocked: number; cooldowns_cleared: number }>(
      '/api/operations/unblock-jobs',
      {
        method: 'POST',
        body: JSON.stringify({
          error_substring: input.error_substring ?? null,
          clear_provider_cooldowns: input.clear_provider_cooldowns ?? true,
        }),
      }
    ),
  listProviderCooldowns: () =>
    request<{ cooldowns: ProviderCooldown[] }>('/api/operations/provider-cooldowns'),
  clearProviderCooldown: (providerName?: string) =>
    request<{ cleared: number; released: number }>('/api/operations/provider-cooldowns/clear', {
      method: 'POST',
      body: JSON.stringify({ provider_name: providerName ?? null }),
    }),
  releaseScheduledRetries: () =>
    request<{ released: number }>('/api/operations/release-scheduled-retries', {
      method: 'POST',
      body: JSON.stringify({}),
    }),
  audit: (limit?: number, options?: RequestOptions) =>
    request<{ items: AuditEvent[] }>(
      limit ? `/api/audit?limit=${encodeURIComponent(limit)}` : '/api/audit', options
    ),
  auditIntegrity: (options?: RequestOptions) => request<AuditIntegrityReport>('/api/audit/integrity', options),
  applyAuditRetention: () => request<RetentionResult>('/api/audit/retention/apply', { method: 'POST' }),
  prompts: (options?: RequestOptions) => request<{ items: Prompt[] }>('/api/prompts', options),
  promptUsage: (options?: RequestOptions) => request<{ items: PromptUsage[] }>('/api/prompts/usage', options),
  promptExperiments: (options?: RequestOptions) => request<{ items: PromptExperiment[] }>('/api/prompts/experiments', options),
  createPrompt: (input: { stage: Stage; name: string; content: string; output_schema?: unknown; activate?: boolean }) =>
    request<{ id: string }>('/api/prompts', {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  testPrompt: (input: { stage: Stage; content: string; sample_text?: string; paperless_document_id?: number | null; provider_name?: string | null; model?: string | null }) =>
    request<PromptTestResponse>('/api/prompts/test', {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  activatePrompt: (id: string) => request<{ ok: boolean }>(`/api/prompts/${id}/activate`, { method: 'POST' }),
  sessions: (options?: RequestOptions) => request<{ items: SessionItem[] }>('/api/auth/sessions', options),
  revokeSession: (id: string) => request<{ ok: boolean }>(`/api/auth/sessions/${id}/revoke`, { method: 'POST' }),
  changePassword: (current_password: string, new_password: string) =>
    request<{ ok: boolean }>('/api/auth/change-password', {
      method: 'POST',
      body: JSON.stringify({ current_password, new_password })
    }),
  users: (options?: RequestOptions) => request<{ items: UserItem[] }>('/api/users', options),
  createUser: (input: { username: string; email?: string; password: string; roles: Role[] }) =>
    request<{ id: string }>('/api/users', {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  enableUser: (id: string) => request<{ ok: boolean }>(`/api/users/${id}/enable`, { method: 'POST' }),
  disableUser: (id: string) => request<{ ok: boolean }>(`/api/users/${id}/disable`, { method: 'POST' }),
  updateUserRoles: (id: string, roles: Role[]) =>
    request<{ ok: boolean }>(`/api/users/${id}/roles`, {
      method: 'POST',
      body: JSON.stringify({ roles })
    }),
  resetPassword: (id: string, password: string) =>
    request<{ ok: boolean }>(`/api/users/${id}/reset-password`, {
      method: 'POST',
      body: JSON.stringify({ password })
    }),
  apiTokens: (options?: RequestOptions) => request<{ items: ApiToken[] }>('/api/api-tokens', options),
  createApiToken: (input: { name: string; scopes: string[]; expires_in_days?: number | null }) =>
    request<{ id: string; token: string; expires_at?: string | null }>('/api/api-tokens', {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  rotateApiToken: (id: string, input: { expires_in_days?: number | null }) =>
    request<{ id: string; token: string; expires_at?: string | null }>(`/api/api-tokens/${id}/rotate`, {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  revokeApiToken: (id: string) => request<{ ok: boolean }>(`/api/api-tokens/${id}`, { method: 'DELETE' })
};
