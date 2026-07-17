# Issue #352 Frontend Security Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the vulnerable locked `undici 7.27.2` used by JSDOM with the patched compatible release and restore the frontend audit gate.

**Architecture:** Keep `jsdom 29.1.1` and its declared `undici ^7.25.0` contract. Refresh only the transitive lockfile resolution to `undici 7.28.0`; do not add an override because the upstream range already admits the patched version.

**Tech Stack:** pnpm 10, JSDOM 29, Undici 7, Vitest 4, TypeScript 6, Vite 8.

## Global Constraints

- Implement GitLab issue #352 only.
- Do not reduce the audit severity and do not mute advisories.
- Preserve the frozen pnpm lockfile workflow.
- Treat `pnpm audit --audit-level high` as the failing security-contract test.
- Use the repository's actual test script, `pnpm test:a11y`; no `pnpm test` script exists.

---

### Task 1: Capture the failing dependency contract

**Files:**
- Inspect: `frontend/package.json`
- Inspect: `frontend/pnpm-lock.yaml`

**Interfaces:**
- Consumes: `jsdom 29.1.1 -> undici 7.27.2`.
- Produces: a reproducible RED audit and the exact safe upstream range.

- [ ] **Step 1: Reproduce the high-severity audit failure**

Run from `frontend/`:

```bash
pnpm audit --audit-level high
```

Expected: exit 1 for three high-severity Undici advisories fixed in `>=7.28.0`.

- [ ] **Step 2: Confirm the transitive path and upstream range**

Run:

```bash
pnpm why undici
pnpm view jsdom@29.1.1 dependencies --json
pnpm view undici@7.28.0 version engines
```

Expected: the only path is JSDOM; JSDOM declares `undici ^7.25.0`, so 7.28.0 is compatible.

### Task 2: Refresh only the vulnerable lockfile resolution

**Files:**
- Modify: `frontend/pnpm-lock.yaml`

**Interfaces:**
- Consumes: the existing JSDOM semver range.
- Produces: a frozen lockfile resolving Undici 7.28.0 without a policy override.

- [ ] **Step 1: Update the transitive package within its declared range**

Run:

```bash
pnpm update undici@7.28.0
```

Expected: only the Undici lockfile entries change from 7.27.2 to 7.28.0.

- [ ] **Step 2: Verify the dependency graph and security gate**

Run:

```bash
pnpm why undici
pnpm audit --audit-level high
```

Expected: Undici 7.28.0 is the sole resolved version and no high-severity advisory remains.

### Task 3: Run the complete frontend gate and commit #352

**Files:**
- Verify: `frontend/package.json`
- Verify: `frontend/pnpm-lock.yaml`

**Interfaces:**
- Consumes: the patched frozen dependency graph.
- Produces: a reviewable #352 commit with the full frontend verification green.

- [ ] **Step 1: Verify the frozen install**

Run:

```bash
pnpm install --frozen-lockfile
```

Expected: exit 0 with an unchanged lockfile.

- [ ] **Step 2: Run type, test, and production-build gates**

Run:

```bash
pnpm typecheck
pnpm test:a11y
pnpm build
```

Expected: all commands pass.

- [ ] **Step 3: Inspect and commit only #352 files**

Run from the repository root:

```bash
git diff --check
git diff -- frontend/pnpm-lock.yaml
git add frontend/pnpm-lock.yaml
git commit -m "fix(frontend): update patched undici lockfile"
```

Expected: one minimal lockfile-only dependency commit; the plan remains in its preceding documentation commit.
