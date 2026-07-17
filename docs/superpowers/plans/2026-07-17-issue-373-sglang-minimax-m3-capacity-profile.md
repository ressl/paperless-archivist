# Issue #373: SGLang/MiniMax M3 Capacity Profile Plan

**Goal:** Measure the pinned SGLang/MiniMax M3 contract with bounded synthetic workloads, derive safe and identical Core/frontend tuning defaults, and prove timeout, lease, concurrency, retry, and interactive-burst behavior without exposing private infrastructure.

**Safety contract:** Load runs are opt-in, bounded by request count, concurrency, timeout, response size, and exact model pins. Prompts contain only synthetic data. Reports contain aggregate timings and public runtime pins, never endpoint URLs, credentials, prompt bodies, response bodies, hostnames, namespaces, or private topology. No runtime or hardware configuration is changed.

### Task 1: Lock the capacity-harness contract

**Files:**
- Add: `scripts/verify/sglang_minimax_m3_capacity.test.mjs`
- Add: `scripts/verify/sglang_minimax_m3_capacity.mjs`
- Modify: `frontend/package.json`

- [x] Add failing offline tests for exact model/runtime pins, synthetic Worker Metadata and Document Chat payloads, bounded concurrency/count/timeout/body size, and secret-safe reporting.
- [x] Reuse the live-contract configuration rules: explicit opt-in, environment/file-based credentials, normalized `/v1`, and exit codes `0` pass, `1` contract/capacity failure, `2` configuration failure.
- [x] Implement sequential, parallel, and mixed Worker Metadata plus Document Chat scenarios with throughput, p50/p95, timeout rate, and error rate.
- [x] Keep the ordinary `pnpm test` gate offline; make the real load run an explicit operator command only.

### Task 2: Measure and choose the preset

**Files:**
- Add: `docs/performance/2026-07-17-sglang-minimax-m3-capacity.md`

- [x] Run the bounded harness against the reviewed pinned deployment without changing it.
- [x] Record only aggregate, public-safe results and the exact public model/runtime pins.
- [x] Choose and justify worker concurrency, request timeout, output limit, and structured-output mode from the measurements while reserving interactive burst capacity.
- [x] State the measurement limits and a repeatable operator revalidation procedure.

### Task 3: Apply identical Core/frontend defaults

**Files:**
- Modify: `crates/archivist-core/src/lib.rs`
- Modify: `frontend/src/modelCatalog.ts`
- Modify: `frontend/src/settings/sections/tuning.tsx`
- Modify/add: focused Rust and frontend tests

- [x] Add failing parity tests for the exact M3 tuning profile.
- [x] Put the measured values in `AiProviderSettings::sglang_minimax_m3_default()` and the built-in frontend provider.
- [x] Give Reset-to-defaults an M3-specific tuning kind instead of the generic OpenAI-compatible preset.
- [x] Preserve disabled-by-default, text-only, exact-model, and no-reasoning behavior.

### Task 4: Prove operational safety

**Files:**
- Modify: `docs/PERFORMANCE.md`
- Modify: `docs/PROVIDER_TUNING_PLAN.md`
- Modify: focused worker tests if the existing seams do not cover the measured preset

- [x] Prove the provider value cannot exceed the global worker env cap and that a transition retains its existing audit signal.
- [x] Prove the job lease is longer than the measured preset timeout plus the safety margin.
- [x] Document bounded retry ceilings and the existing retry/error/latency/queue-age metrics used to diagnose provider pressure.
- [x] Document interactive burst behavior and whether the measurement requires additional application backpressure.

### Task 5: Verify and deliver

- [x] Run offline harness tests, the bounded live load, frontend tests/lint/typecheck, focused Rust tests, formatting, documentation links, and secret scan.
- [x] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify branch and MR pipelines, document aggregate evidence, and close #373 only when green.
