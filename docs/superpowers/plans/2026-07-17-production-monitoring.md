# Production Monitoring Implementation Plan

**Goal:** Complete issue #311 with a correct permanent-failure counter,
authenticated Prometheus Operator integration, tested alerts, and a verified
production rollout.

**Design:**
[`2026-07-17-production-monitoring-design.md`](../specs/2026-07-17-production-monitoring-design.md)

## Task 1: Add a monotone permanent-job-failure counter

**Files**

- Add: `crates/archivist-db/tests/job_failure_metrics.rs`
- Add: `migrations/0050_job_failure_counter.sql`
- Modify: `crates/archivist-db/src/lib.rs`
- Modify: `crates/archivist-api/src/main.rs`

- [ ] Write a DB integration test proving one permanent transition increments
      `job_failures_total`, while a retry and a lost lease do not.
- [ ] Run the focused test and capture the expected RED failure because the
      counter does not exist.
- [ ] Seed `job_failures_total` in `metrics_counters` and increment it inside
      the successful permanent-failure transaction.
- [ ] Export `paperless_archivist_job_failures_total` as a Prometheus counter.
- [ ] Run the focused test and relevant DB/API suites to GREEN.
- [ ] Commit the counter and regression test.

## Task 2: Lock down and document the metrics HTTP contract

**Files**

- Modify: `crates/archivist-api/src/main.rs`
- Modify: `openapi/openapi.yaml`
- Modify: `docs/API_REFERENCE.md`

- [ ] Add failing unit tests for unset, missing, wrong, and valid dedicated
      metrics bearer tokens, including a non-disclosure assertion.
- [ ] Extract the constant-time authorization decision into a small helper used
      by `/metrics` without changing response semantics.
- [ ] Add a dedicated OpenAPI metrics-bearer scheme plus explicit 401 and 503
      responses.
- [ ] Run API unit tests and OpenAPI client generation to GREEN.
- [ ] Commit the endpoint contract.

## Task 3: Add the public opt-in monitoring component test-first

**Files**

- Add: `scripts/verify/kubernetes_monitoring_contract.test.mjs`
- Add: `scripts/verify/paperless_archivist_alert_rules.yaml`
- Add: `scripts/verify/paperless_archivist_alert_rules.test.yaml`
- Add: `deploy/kubernetes/components/monitoring/kustomization.yaml`
- Add: `deploy/kubernetes/components/monitoring/deployment-api-metrics-env-patch.yaml`
- Add: `deploy/kubernetes/components/monitoring/servicemonitor.yaml`
- Add: `deploy/kubernetes/components/monitoring/prometheusrule.yaml`
- Add: `deploy/kubernetes/examples/monitoring/kustomization.yaml`
- Modify: `frontend/package.json`
- Modify: `.gitlab-ci.yml`
- Modify: `.github/workflows/ci.yml`

- [ ] Write the Node contract test first and run it RED against missing files.
- [ ] Add an opt-in Kustomize component that references a dedicated Secret,
      selects only the API Service, and contains queue, scrape-down,
      permanent-failure-rate, and quota alerts.
- [ ] Keep a raw rule fixture synchronized with the PrometheusRule spec so
      `promtool` validates exactly the supported expressions.
- [ ] Add a synthetic quota-counter increase and a flat-counter quiet case.
- [ ] Run Node tests, both Kustomize renders, `promtool check rules`, and
      `promtool test rules` with Prometheus 3.12.0.
- [ ] Add the checks to both supported CI definitions and commit.

## Task 4: Correct public operations documentation

**Files**

- Modify: `README.md`
- Modify: `deploy/kubernetes/README.md`
- Modify: `deploy/kubernetes/secret.example.yaml`
- Modify: `deploy/kubernetes/values.example.yaml`
- Modify: `docs/OPERATIONS.md`
- Modify: `docs/TESTING_ARCHITECTURE.md`

- [ ] Remove every unauthenticated `/metrics` example and show a secret-file
      based bearer request that does not expose a real token in arguments.
- [ ] Document the dedicated Secret, opt-in component, namespace/label patches,
      alerts, validation, rotation, and rollback.
- [ ] Run link, secret/boundary, YAML, and render checks; commit.

## Task 5: Implement and test the private deployment wiring

**Private deployment files**

- Add: `k8s/app/servicemonitor.yaml`
- Add: `k8s/app/prometheusrule.yaml`
- Add: `k8s/secrets/paperless-archivist-metrics.sops.yaml`
- Add: `tests/fixtures/paperless-archivist-alert-rules.yaml`
- Add: `tests/fixtures/paperless-archivist-alert-rules.test.yaml`
- Add: `tests/paperless_archivist_monitoring_contract.rb`
- Modify: `k8s/app/api-deployment.yaml`
- Modify: `k8s/app/networkpolicy.yaml`
- Modify: `k8s/app/kustomization.yaml`
- Modify: `k8s/secrets/kustomization.yaml`
- Modify: `k8s/secrets/README.md`
- Modify: `.gitlab-ci.yml`

- [ ] Use a fresh isolated checkout; preserve unrelated local changes.
- [ ] Write and run the manifest contract test RED.
- [ ] Generate the token with a cryptographic RNG and encrypt it directly into
      the dedicated SOPS Secret without printing plaintext.
- [ ] Wire only the API and ServiceMonitor to the same Secret key.
- [ ] Admit only the observed Prometheus pod identity on TCP/8080.
- [ ] Add the four alerts and synchronized raw rule fixture.
- [ ] Run Ruby contract, YAML, SOPS-shape, Kustomize, and Prometheus 3.12.0 rule
      checks locally; add equivalent pipeline gates.
- [ ] Commit, push, merge after a green private deploy pipeline.

## Task 6: Roll out and prove production behavior

- [ ] Wait for Argo CD sync and API rollout health.
- [ ] Verify unset behavior before rollout was 503; after rollout verify missing
      and wrong tokens are 401 and a secret-safe valid-token probe is 200.
- [ ] Verify Prometheus reports the target `up == 1` under the active policy.
- [ ] Verify all rules load without errors and the synthetic quota fixture
      reaches firing state.
- [ ] Scan diffs, history, rendered resources, and logs for credential leakage.
- [ ] Attach concise evidence to #311 and close it only after every criterion is
      proven.
