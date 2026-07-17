# Issue #371: SGLang MiniMax M3 Live Contract Plan

**Goal:** Add an opt-in, public-safe contract harness that can prove the pinned SGLang/MiniMax M3 runtime one contract at a time without making private infrastructure or credentials part of normal merge-request CI.

**Safety contract:** Endpoint, contract selection, timeouts, fingerprint metadata, output path, and vision scope come only from environment variables. Authentication comes only from a referenced secret file. The report never contains the endpoint or credential; it identifies the endpoint by SHA-256 and bounds/redacts every provider error snippet.

### Task 1: Define the offline harness contract

**Files:**
- Add: `scripts/verify/sglang_minimax_m3_contract.mjs`
- Add: `scripts/verify/sglang_minimax_m3_contract.test.mjs`

- [ ] Add failing mock tests for individual selection, stable exit codes, exact model discovery, text, strict schema, disabled/enabled/adaptive thinking, forced tool calls, and image input.
- [ ] Add negative mocks for wrong model, schema HTTP 400, reasoning-only output, missing tool call, invalid image, oversized response, timeout, and redaction.
- [ ] Keep all prompts and the embedded synthetic PNG free of personal data.

### Task 2: Implement the bounded secret-safe runner

**Files:**
- Add: `scripts/verify/sglang_minimax_m3_contract.mjs`

- [ ] Require `SGLANG_CONTRACT_BASE_URL`; default only the public exact model and the accepted public model/runtime/image fingerprints.
- [ ] Read an optional API key only from `SGLANG_CONTRACT_API_KEY_FILE`; never accept it as an argument or print it.
- [ ] Run each selected contract independently with an explicit timeout and response-size ceiling.
- [ ] Emit a versioned JSON report with status, latency, safe diagnostics, observed model identity, public fingerprints, and endpoint SHA-256.
- [ ] Make image failures informational by default and gating only under explicit vision scope.

### Task 3: Wire offline and protected CI paths

**Files:**
- Modify: `.gitlab-ci.yml`

- [ ] Run all mock/negative tests in ordinary offline pipelines.
- [ ] Add a manual live job only for protected refs with an operator-supplied endpoint.
- [ ] Persist the redacted JSON report even when a live contract fails.
- [ ] Keep normal merge requests independent of any private endpoint, credentials, GPU, or network path.

### Task 4: Document and prove the pinned runtime

**Files:**
- Modify: `docs/TESTING_ARCHITECTURE.md`

- [ ] Document every environment variable, separate-contract invocation, exit-code rule, vision-scope rule, report field, and troubleshooting boundary.
- [ ] Run mock tests, docs links, YAML/config validation, Rust/frontend regression gates, formatting, and secret scans.
- [ ] Run the full harness against the pinned live SGLang/M3 deployment and record only the public/redacted report evidence in #371.
- [ ] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify both GitLab pipelines, document evidence, and close #371 only when green.
