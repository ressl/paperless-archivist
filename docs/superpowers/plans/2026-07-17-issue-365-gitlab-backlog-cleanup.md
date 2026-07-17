# Issue #365: GitLab Backlog Cleanup Plan

**Goal:** Make active milestones and confidential deploy-feedback issues reflect real remaining work without deleting history or publishing private deployment details.

### Task 1: Capture and classify the before state

**Files:**
- Add: `docs/audits/2026-07-17-gitlab-backlog-cleanup.md`

- [ ] Record every active milestone with its open/closed issue counts.
- [ ] Record all confidential deploy-feedback issues by release and failed validation phase without copying private details.
- [ ] Verify current source/deployment evidence and identify any reproducible residual work.

### Task 2: Close only completed milestones

- [ ] Close all 17 active milestones whose assigned issues are all closed.
- [ ] Keep milestones #19, #48, #50, and #51 active and document their concrete open successors.
- [ ] Re-query GitLab and prove that every remaining active milestone has open work.

### Task 3: Consolidate confidential deploy feedback

- [ ] Group the 29 historical issues by validation phase and release lineage.
- [ ] Link every historical issue to a confidential canonical consolidation record before closing it.
- [ ] Preserve any reproducible residual work in a current canonical issue; otherwise document the superseding healthy evidence.
- [ ] Re-query GitLab and prove no stale deploy-feedback issue remains open.

### Task 4: Verify and deliver

- [ ] Add the after state and a link/audit sample to the committed report without confidential bodies or infrastructure details.
- [ ] Run Markdown/repository checks and obtain an independent Critical/Important review.
- [ ] Commit/push, verify both GitLab pipelines, document evidence, and close #365 only when green.
