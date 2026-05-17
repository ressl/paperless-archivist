-- v1.5.15 — Bundle E #119: A/B prompt experiment groups.
--
-- Currently, only one prompt row per (stage, name) can have active=true,
-- and `get_active_prompt(stage)` returns that single winner. This makes
-- prompt iteration "all-or-nothing" — flip a new version active, hope
-- it works, flip back if not.
--
-- This migration adds an `experiment_group` column so two rows for the
-- same (stage, name) can BOTH be active at once: one tagged
-- experiment_group='A', the other 'B'. The worker routes runs to the
-- A or B prompt deterministically by hash(run_id) % 2, then surfaces
-- which group was used in `ai_artifacts.normalized.prompt_experiment_group`
-- so dashboards can compare approval-rate / dropped-field-count /
-- consensus-disagreement-rate across the two variants and pick a
-- winner empirically.
--
-- Rows without an experiment_group continue to behave exactly as before:
-- the unique partial index `prompts_one_active_per_stage_name_idx` is
-- replaced with one that PARTITIONS by experiment_group so each of
-- (null, 'A', 'B') can hold one active row independently. NULL group
-- means "the default winner" — used by the single-prompt path.

alter table prompts
  add column if not exists experiment_group text;

alter table prompts
  add constraint prompts_experiment_group_check
  check (experiment_group is null or experiment_group in ('A', 'B'));

-- Replace the old "one active per (stage, name)" index with the
-- group-aware version. The drop-if-exists is safe because the new
-- partial index covers the same (active=true) rows and explicitly
-- adds experiment_group to the key.
drop index if exists prompts_one_active_per_stage_name_idx;

create unique index if not exists prompts_one_active_per_stage_name_group_idx
  on prompts (stage, name, coalesce(experiment_group, ''))
  where active;
