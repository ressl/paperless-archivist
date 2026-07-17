# Issue #374: SGLang/MiniMax M3 Operations Documentation Plan

**Goal:** Give operators one public-safe, source-code-independent path to configure, validate, operate, and troubleshoot the exact supported SGLang/MiniMax M3 integration.

**Safety contract:** Documentation contains only public model/runtime pins and `.example.invalid` endpoints. Credentials remain secret references or mounted files. MiniMax M3 remains text-only; MinerU/Ollama remain the OCR providers until the ADR-014 vision gate and issues #322 through #338 are complete.

### Task 1: Consolidate the operator path

**Files:**
- Modify: `docs/API_REFERENCE.md`
- Modify: `docs/USER_GUIDE.md`
- Modify: `docs/INSTALLATION.md`
- Modify: `docs/OPERATIONS.md`

- [x] Replace the outdated M2.7 example and removed API-only tuning limitation with the disabled M3 preset workflow.
- [x] Document exact model identity, provider kind, public-safe URL, secret references, thinking mapping, output cap, structured-output fallback, timeout, and model discovery.
- [x] Link the opt-in Kubernetes NetworkPolicy component, live contract, and measured capacity profile.
- [x] Add symptom-to-diagnosis remediation for model identity, reasoning-only output, leaked tags, schema 400, timeout, policy drop, and authentication.

### Task 2: Align architectural and historical references

**Files:**
- Modify: `docs/FEATURE_REFERENCE.md`
- Modify: `docs/PERFORMANCE.md`
- Modify: `docs/TESTING_ARCHITECTURE.md`
- Modify: `docs/superpowers/specs/2026-07-07-mineru-sglang-providers-design.md`
- Modify: `docs/RELEASE_NOTES.md`

- [x] Make the text-only M3 versus MinerU/Ollama OCR boundary and #322 through #338 gate explicit.
- [x] Cross-link the live smoke, capacity evidence, and troubleshooting runbook without duplicating mutable runtime details.
- [x] Mark all superseded M2.7 examples in the historical design and record the completed operational documentation in release notes.

### Task 3: Verify and deliver

- [x] Run the documentation link checker and validate every documented command against script help/configuration behavior.
- [x] Scan changed documentation for credentials, private hosts, private registries, and internal-only URLs.
- [x] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify branch and MR pipelines, document evidence, and close #374 only when green.
