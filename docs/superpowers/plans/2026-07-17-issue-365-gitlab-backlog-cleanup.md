# Issue #365: GitLab Backlog Cleanup Plan

**Goal:** Make active milestones and confidential deploy-feedback issues reflect real remaining work without deleting history or publishing private deployment details.

### Task 1: Capture and classify the before state

**Files:**
- Add: `docs/audits/2026-07-17-gitlab-backlog-cleanup.md`

- [x] Record every active milestone with its open/closed issue counts.
- [x] Record all confidential deploy-feedback issues by release and failed validation phase without copying private details.
- [x] Verify current source/deployment evidence and identify any reproducible residual work.

### Task 2: Close only completed milestones

- [x] Close all 17 active milestones whose assigned issues are all closed.
- [x] Keep milestones #19, #48, #50, and #51 active and document their concrete open successors.
- [x] Re-query GitLab and prove that every remaining active milestone has open work.

### Task 3: Consolidate confidential deploy feedback

- [x] Group the 29 historical issues by validation phase and release lineage.
- [x] Link every historical issue to a confidential canonical consolidation record before closing it.
- [x] Preserve any reproducible residual work in a current canonical issue; otherwise document the superseding healthy evidence.
- [x] Re-query GitLab and prove no stale deploy-feedback issue remains open.

### Task 4: Verify and deliver

- [x] Add the after state and a link/audit sample to the committed report without confidential bodies or infrastructure details.
- [x] Run Markdown/repository checks and obtain an independent Critical/Important review.
- [ ] Commit/push, verify both GitLab pipelines, document evidence, and close #365 only when green.
