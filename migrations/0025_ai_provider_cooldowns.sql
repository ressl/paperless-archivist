-- v1.5.27 — quota-aware backoff for AI providers.
--
-- Records a per-provider "do not call until" timestamp. The worker writes
-- this when an AI provider replies 429 with a usage-limit signal (Ollama
-- Cloud weekly cap, OpenAI tier quotas, etc.); the active claim loop
-- checks it before processing a job and releases the lease back to the
-- queue if the active stage's provider is still cooled down. Cooldowns
-- are not retried via the existing per-job backoff (which would burn
-- through `max_attempts` against a quota that doesn't reset for days).
--
-- One row per provider name (matches `runtime.ai.providers[].name`).
-- Updated in place via `INSERT … ON CONFLICT (provider_name) DO UPDATE`.

CREATE TABLE IF NOT EXISTS ai_provider_cooldowns (
    provider_name   text        PRIMARY KEY,
    cooldown_until  timestamptz NOT NULL,
    reason          text        NOT NULL,
    set_at          timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- No secondary index: the primary-key lookup by `provider_name` covers
-- the hot path (`get_active_provider_cooldown` for a given provider),
-- and the row count is one per configured AI provider — typically ≤5
-- — so the sequential scan in `list_active_provider_cooldowns` is
-- cheaper than maintaining an index. A partial index keyed on
-- `cooldown_until > now()` would have been ideal but Postgres rejects
-- it: `functions in index predicate must be marked IMMUTABLE`, and
-- `now()` is STABLE. A full b-tree on `cooldown_until` is also not
-- worth the write cost given the table size.
