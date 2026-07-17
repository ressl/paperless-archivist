# Issue #372: Opt-in Custom AI Egress Component Plan

**Goal:** Provide a public-safe, additive Kustomize component that grants the Archivist API and worker egress only to explicitly labelled custom-AI pods on explicit example ports, while leaving the restrictive base policy unchanged.

**Safety contract:** The base manifest is not edited. Every added peer combines a non-empty namespace selector with a non-empty pod selector and a TCP port. The example contains no private hostname, CIDR, registry, namespace, or production topology; operators must replace its public example labels and ports in their own overlay.

### Task 1: Lock the least-privilege YAML contract

**Files:**
- Add: `scripts/verify/kubernetes_custom_ai_egress_contract.test.mjs`
- Modify: `frontend/package.json`

- [x] Add failing assertions that the base does not contain the example provider ports.
- [x] Require the opt-in policy to select both `api` and `worker`, with no other Archivist component.
- [x] Require every peer to combine an exact namespace label and exact provider pod label; reject empty selectors, `ipBlock`, and unrestricted ports.
- [x] Assert the public SGLang and MinerU examples use only their documented example TCP ports.
- [x] Make the static contract part of the ordinary offline `pnpm test` gate.

### Task 2: Add the component and renderable example

**Files:**
- Add: `deploy/kubernetes/components/custom-ai-egress/kustomization.yaml`
- Add: `deploy/kubernetes/components/custom-ai-egress/networkpolicy.yaml`
- Add: `deploy/kubernetes/examples/custom-ai-egress/kustomization.yaml`

- [x] Add an additive `NetworkPolicy`; do not modify the base policy or deployments.
- [x] Select both backend components through existing stable pod labels.
- [x] Add separate SGLang and MinerU peers with namespace/pod selector intersection and explicit TCP ports.
- [x] Compose base plus component through an opt-in example Kustomization.
- [x] Render base and example with `kubectl kustomize` and assert both policies occur only in the opt-in render.

### Task 3: Document safe adaptation and diagnosis

**Files:**
- Modify: `deploy/kubernetes/README.md`
- Modify: `docs/OPERATIONS.md`
- Modify: `.gitlab-ci.yml`

- [x] Document render, diff, label, port, apply, and connectivity verification commands.
- [x] State that all names and ports are examples and must match the operators own Service target ports and pod labels.
- [x] Explain additive policy semantics, API/worker coverage, common policy-drop symptoms, DNS/service/endpoints checks, and CNI flow-log diagnosis.
- [x] Add a public render job without introducing private topology or weakening normal offline CI.

### Task 4: Verify and deliver

- [x] Run static contract tests, base/opt-in renders, YAML assertions, docs links, CI lint, formatting, and secret scan.
- [x] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify branch and MR pipelines, document evidence, and close #372 only when green.
