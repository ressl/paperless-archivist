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

CREATE INDEX IF NOT EXISTS ai_provider_cooldowns_active_idx
    ON ai_provider_cooldowns (cooldown_until)
    WHERE cooldown_until > now();
