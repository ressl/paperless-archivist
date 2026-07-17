# Issue #376: Vision Fallback Lease Fencing Plan

**Goal:** Keep a claimed OCR job fenced across the primary vision request, Ollama model discovery, and the optional fallback request so a lost lease can never lead to a second provider call or a duplicate apply path.

**Safety contract:** The provider timeout still bounds each network call. The worker renews the same owner-scoped database lease before every high-level vision call and stops successfully, without cache writes, audit success, completion, or apply, as soon as ownership is lost. Model choice, fallback order, retry classification, and runtime hardware do not change.

### Task 1: Lock the fencing behavior with tests

**Files:**
- Modify: `crates/archivist-worker/src/main.rs`

- [ ] Add a reusable async test seam that renews a lease before polling a provider future.
- [ ] Add delayed-call tests proving the order primary heartbeat -> primary, discovery heartbeat -> discovery, and fallback heartbeat -> fallback.
- [ ] Add lease-loss tests proving the protected provider future is never polled after a failed renewal.

### Task 2: Fence the production fallback path

**Files:**
- Modify: `crates/archivist-worker/src/main.rs`

- [ ] Renew immediately before the primary call, after a runtime-crash error before discovery, and again before the fallback call.
- [ ] Propagate lease loss as a controlled OCR stop before cache, audit-success, completion, review, or apply writes.
- [ ] Preserve the original runtime-crash error when no fallback exists and the existing retry/error classification when a call fails.

### Task 3: Align Ollama discovery timeout

**Files:**
- Modify: `crates/archivist-worker/src/main.rs`
- Modify/add: focused tests around timeout construction

- [ ] Construct the Ollama discovery client with the resolved `StageProvider.request_timeout_seconds` value.
- [ ] Prove the resolved provider timeout is used instead of the legacy fixed constructor default.

### Task 4: Document operations

**Files:**
- Modify: `docs/PERFORMANCE.md`
- Modify: `docs/RELEASE_NOTES.md`

- [ ] Explain the three independently fenced network steps and the closed behavior on lease loss.
- [ ] Document the provider-timeout/lease relationship and the existing lease-loss log signal.

### Task 5: Verify and deliver

- [ ] Run focused Worker tests, the full Worker suite, Clippy, formatting, documentation checks, and secret scan.
- [ ] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify branch and MR pipelines, document evidence, and close #376 only when green.
