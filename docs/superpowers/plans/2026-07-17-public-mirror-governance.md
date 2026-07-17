# Public Mirror Governance Implementation Plan

**Goal:** Complete issue #61 by making GitHub Actions the canonical public CI,
disabling duplicate CI on the official GitLab.com mirror, and replacing the
temporary export credential with an annually rotated least-privilege identity.

**Design:**
[`2026-07-17-public-mirror-governance-design.md`](../specs/2026-07-17-public-mirror-governance-design.md)

## Task 1: Document the public CI policy

**Files**

- Modify: `CONTRIBUTING.md`
- Modify: `.gitlab-ci.yml`
- Modify: `docs/RELEASE_CHECKLIST.md`

- [ ] State that GitHub Actions is the official public gate, the official
      GitLab.com project is a source mirror with CI disabled, and the portable
      GitLab pipeline remains available to forks.
- [ ] Replace the release requirement for green official GitLab.com CI with
      exact mirror-commit equality and no unexpected mirror pipeline.
- [ ] Run documentation and public-boundary checks; commit.

## Task 2: Create the least-privilege export identity

- [ ] Confirm no equivalent project-scoped service account already exists.
- [ ] Create one project service account and grant only the role required to
      push the protected default branch.
- [ ] Create one PAT with only `write_repository`, expiry 2027-07-16, and a
      mandatory rotation date of 2027-06-16.
- [ ] Pipe its one-time value directly over standard input into the internal
      export variable; never print or persist it.
- [ ] Set the variable protected, masked, hidden, raw, and scoped only to the
      public-export environment. Record owner, purpose, expiry, and rotation in
      its non-secret description.
- [ ] Verify service-account membership, token scope/expiry, and variable
      metadata without reading any secret value.

## Task 3: Disable official GitLab.com project CI

- [ ] Set the mirror project's `builds_access_level` to `disabled`.
- [ ] Verify both `builds_access_level=disabled` and `jobs_enabled=false`.
- [ ] Preserve historical pipelines as audit evidence; do not delete them.

## Task 4: Export and verify

- [ ] Run the protected internal public export after the source merge.
- [ ] Verify the export and all public boundary jobs succeed with no credential
      material in logs.
- [ ] Verify internal, GitHub, and GitLab.com `main` resolve to the same expected
      commit.
- [ ] Verify GitHub Actions succeeds for that commit.
- [ ] Verify the GitLab.com export push creates no new pipeline.
- [ ] Attach evidence to #61 and close it only after all controls are proven.

## Task 5: Close the remaining tracking scope

- [ ] Re-query all open issues, merge requests, and milestones.
- [ ] Close the production-audit milestone only when #311 is closed and no
      other open issue remains assigned to it.
- [ ] Report any intentionally deferred item with its reason; otherwise report
      a clean open-scope result.
