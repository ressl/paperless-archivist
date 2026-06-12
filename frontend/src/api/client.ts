import type { components } from './schema';

export type MetadataTrace = components['schemas']['MetadataTrace'];
export type MetadataTraceRun = components['schemas']['MetadataTraceRun'];
export type MetadataFieldOutcome = components['schemas']['MetadataFieldOutcome'];
export type AiRuntimeHints = components['schemas']['AiRuntimeHints'];
export type AiLoadedModel = components['schemas']['AiLoadedModel'];

// Per-provider tuning block (v1.6.2). Mirrors archivist_core::ProviderTuning.
// All fields are optional: when null/undefined, the global setting in
// workflow / ocr / metadata / tagging applies. See docs/PROVIDER_TUNING_PLAN.md.
export type ReasoningEffort = 'off' | 'low' | 'medium' | 'high';

export type ModelUsageTier = 'low' | 'medium' | 'high' | 'extra_high';

// Editable model-picker catalog entry (v1.6.3). Mirrors
// archivist_core::ModelCatalogEntry; persisted in runtime_settings.
export type ModelCatalogEntry = {
  provider_kind: AiProviderKind;
  capability: 'text' | 'vision';
  model_id: string;
  label?: string | null;
  recommended: boolean;
  usage_tier?: ModelUsageTier | null;
  context?: string | null;
  modality?: string | null;
  best_for?: string | null;
};

export type ProviderTuning = {
  worker_concurrency?: number | null;
  consensus_secondary_text_model?: string | null;
  consensus_date_tolerance_days?: number | null;
  text_num_ctx?: number | null;
  vision_num_ctx?: number | null;
  reasoning_effort?: ReasoningEffort | null;
  ocr_page_limit?: number | null;
  hourly_document_limit?: number | null;
  daily_document_limit?: number | null;
  metadata_confidence_threshold?: number | null;
  title_confidence_threshold?: number | null;
  correspondent_confidence_threshold?: number | null;
  document_type_confidence_threshold?: number | null;
  document_date_confidence_threshold?: number | null;
  tags_confidence_threshold?: number | null;
  fields_confidence_threshold?: number | null;
  max_tags?: number | null;
  allowed_list_max?: number | null;
  request_timeout_seconds?: number | null;
};

export type Role = 'viewer' | 'reviewer' | 'operator' | 'admin' | 'auditor';
export type Stage = 'ocr' | 'metadata';
export type PipelineStage = Stage | 'apply';
export type ProcessingMode = 'manual_review' | 'auto_select_review' | 'full_auto';
export type AiProviderKind = 'ollama' | 'openai' | 'anthropic' | 'openai_compatible';

export type AiProvider = {
  name: string;
  kind: AiProviderKind;
  base_url: string;
  default_text_model?: string | null;
  default_vision_model?: string | null;
  cost_per_1m_input_tokens_usd?: number | null;
  cost_per_1m_output_tokens_usd?: number | null;
  secret_id?: string | null;
  enabled: boolean;
  tuning?: ProviderTuning;
};

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

export type RuntimeSettings = {
  paperless: {
    base_url: string;
    public_url?: string | null;
    token_secret_id?: string | null;
    timeout_seconds: number;
    login_bridge_enabled: boolean;
    delta_sync_enabled: boolean;
    delta_sync_overlap_minutes: number;
    active_archive: string;
    archive_profiles: Array<{
      name: string;
      base_url: string;
      token_secret_id?: string | null;
      enabled: boolean;
    }>;
  };
  ai: {
    default_provider: string;
    ollama_base_url: string;
    default_text_model: string;
    default_vision_model: string;
    stage_models: Array<{ stage: Stage; provider: string; model: string }>;
    providers: AiProvider[];
    external_provider_warning_acknowledged: boolean;
    fallback_vision_model?: string | null;
    requeue_vision_crashes_on_startup?: boolean;
    ollama_vision_num_ctx?: number;
    ollama_text_num_ctx?: number;
    model_catalog: ModelCatalogEntry[];
  };
  security: {
    audit_retention_days: number;
    ai_artifact_retention_days: number;
    runs_retention_days: number;
    ai_artifact_storage: 'full' | 'redacted' | 'metadata_only';
    api_token_expiry_required: boolean;
    api_token_default_ttl_days: number;
    api_token_max_ttl_days: number;
  };
  notifications: {
    enabled: boolean;
    webhook_url_secret_id?: string | null;
    review_queue_threshold: number;
    repeated_failure_threshold: number;
    cooldown_minutes: number;
  };
  workflow: {
    mode: ProcessingMode;
    paused: boolean;
    dry_run: boolean;
    hourly_document_limit?: number | null;
    daily_document_limit?: number | null;
    tags: Record<string, string>;
    rules: {
      include_tags: string[];
      exclude_tags: string[];
    };
    enabled_stages: Stage[];
    fallback_to_review_on_validation_failure: boolean;
  };
  ocr: {
    page_limit: number;
    min_chars: number;
    renderer: string;
    language_hint?: string | null;
  };
  tagging: {
    max_tags: number;
    allow_new_tags: boolean;
    confidence_threshold: number;
    old_tag_strategy: string;
    tag_output_language: string;
  };
  metadata: {
    overwrite_existing_correspondent: boolean;
    overwrite_existing_document_type: boolean;
    overwrite_existing_document_date: boolean;
    allow_new_correspondents: boolean;
    allow_new_document_types: boolean;
    confidence_threshold: number;
    document_date_confidence_threshold: number;
    title_confidence_threshold?: number;
    correspondent_confidence_threshold?: number;
    document_type_confidence_threshold?: number;
    tags_confidence_threshold?: number;
    fields_confidence_threshold?: number;
    allowed_list_max?: number;
    document_date_anchor_required?: boolean;
    document_date_anchor_penalty?: number;
  };
  fields: {
    max_fields: number;
    confidence_threshold: number;
    mappings: Array<{
      field_name: string;
      enabled: boolean;
      aliases: string[];
      instructions?: string | null;
    }>;
  };
  ui?: {
    debug_console_enabled?: boolean;
  };
};

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

export type DashboardResponse = {
  counts: Counts;
  stats: DashboardStats;
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
  parsed?: unknown;
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

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (init.body && !headers.has('content-type')) {
    headers.set('content-type', 'application/json');
  }
  const method = (init.method ?? 'GET').toUpperCase();
  const csrf = csrfToken();
  if (csrf && !['GET', 'HEAD', 'OPTIONS'].includes(method)) {
    headers.set('x-csrf-token', csrf);
  }
  const response = await fetch(path, {
    ...init,
    credentials: 'include',
    headers
  });
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
      if (body.error) message = body.error;
    } catch {
      // ignore non-JSON errors
    }
    // A 401 on any call but the login attempts themselves means the session
    // expired; notify the app so it returns to the login screen.
    if (response.status === 401 && !path.startsWith('/api/auth/')) {
      unauthorizedHandler?.();
    }
    throw new Error(message);
  }
  const text = await response.text();
  return text ? (JSON.parse(text) as T) : (undefined as T);
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
  oidcConfig: () => request<OidcConfig>('/api/auth/oidc/config'),
  logout: () => request<{ ok: boolean }>('/api/auth/logout', { method: 'POST' }),
  me: () => request<Me>('/api/auth/me'),
  settings: () => request<RuntimeSettings>('/api/settings'),
  saveSettings: (settings: RuntimeSettings, paperlessToken?: string, providerSecrets?: Record<string, string>, notificationWebhookUrl?: string) =>
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
  testProvider: () => request<{ ok: boolean; error?: string; details?: unknown }>('/api/model-providers/test', { method: 'POST' }),
  ollamaModels: (providerName: string) =>
    request<{ provider: string; models: OllamaInstalledModel[] }>(`/api/model-providers/${encodeURIComponent(providerName)}/models`, { method: 'POST' }),
  aiRuntimeHints: (provider?: string) => {
    const query = provider ? `?provider=${encodeURIComponent(provider)}` : '';
    return request<AiRuntimeHints>(`/api/ai/runtime-hints${query}`);
  },
  syncPaperless: () => request<Record<string, unknown>>('/api/paperless/sync-metadata', { method: 'POST' }),
  paperlessConsistency: () => request<PaperlessConsistencyResult>('/api/paperless/consistency'),
  reconcileCompletionTags: (input: { dry_run?: boolean; document_ids?: number[] }) =>
    request<CompletionTagReconcileResult>('/api/paperless/completion-tags/reconcile', {
      method: 'POST',
      body: JSON.stringify(input)
    }),
  dashboard: (range: DashboardRange = '24h') => request<DashboardResponse>(`/api/dashboard?range=${encodeURIComponent(range)}`),
  statistics: (params: StatisticsQueryParams = {}) => {
    const qs = new URLSearchParams();
    if (params.from) qs.set('from', params.from);
    if (params.to) qs.set('to', params.to);
    if (params.bucket) qs.set('bucket', params.bucket);
    const query = qs.toString();
    return request<StatisticsResponse>(`/api/statistics${query ? `?${query}` : ''}`);
  },
  dashboardLive: () => request<DashboardLiveStatus>('/api/dashboard/live'),
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
  inventory: (params: InventoryQueryParams = {}) => {
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
      `/api/inventory?${qs.toString()}`
    );
  },
  inventoryDuplicates: () =>
    request<{ groups: DuplicateGroup[]; paperless_base: string }>('/api/inventory/duplicates'),
  inventoryMetadataTrace: (documentId: number) =>
    request<MetadataTrace>(`/api/inventory/${documentId}/metadata-trace`),
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
  chatSessions: () => request<{ items: DocumentChatSession[] }>('/api/chat/sessions'),
  createChatSession: (title?: string) =>
    request<{ id: string; title: string }>('/api/chat/sessions', {
      method: 'POST',
      body: JSON.stringify({ title: title || null })
    }),
  chatMessages: (id: string) => request<{ items: DocumentChatMessage[] }>(`/api/chat/sessions/${id}`),
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
  reviews: (limit = 100) =>
    request<{ items: ReviewItem[]; total: number; has_more: boolean }>(
      `/api/reviews?status=pending&limit=${encodeURIComponent(String(limit))}`
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
  audit: (limit?: number) =>
    request<{ items: AuditEvent[] }>(
      limit ? `/api/audit?limit=${encodeURIComponent(limit)}` : '/api/audit'
    ),
  auditIntegrity: () => request<AuditIntegrityReport>('/api/audit/integrity'),
  applyAuditRetention: () => request<RetentionResult>('/api/audit/retention/apply', { method: 'POST' }),
  prompts: () => request<{ items: Prompt[] }>('/api/prompts'),
  promptUsage: () => request<{ items: PromptUsage[] }>('/api/prompts/usage'),
  promptExperiments: () => request<{ items: PromptExperiment[] }>('/api/prompts/experiments'),
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
  sessions: () => request<{ items: SessionItem[] }>('/api/auth/sessions'),
  revokeSession: (id: string) => request<{ ok: boolean }>(`/api/auth/sessions/${id}/revoke`, { method: 'POST' }),
  changePassword: (current_password: string, new_password: string) =>
    request<{ ok: boolean }>('/api/auth/change-password', {
      method: 'POST',
      body: JSON.stringify({ current_password, new_password })
    }),
  users: () => request<{ items: UserItem[] }>('/api/users'),
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
  apiTokens: () => request<{ items: ApiToken[] }>('/api/api-tokens'),
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
