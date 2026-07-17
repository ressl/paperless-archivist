# Issue #286: Document Chat Session Race Plan

**Goal:** Ensure only the newest message request for the currently selected Document Chat session may update messages or surface an error, including the slow refresh after sending an LLM request.

**Safety contract:** Session switching is immediate and does not cancel or alter server-side chat generation. Stale client responses and errors are ignored at the state-commit boundary. No chat content, session API contract, streaming behavior, or navigation design changes.

### Task 1: Reproduce both stale-response races

**Files:**
- Add: `frontend/src/chat/DocumentChat.race.test.tsx`

- [ ] Add controlled Deferred-Promise tests for initial A load -> select B -> B resolves -> A resolves.
- [ ] Add the equivalent post-send A refresh -> select B -> B resolves -> A resolves test.
- [ ] Prove stale failures do not call the visible error handler for the new session.

### Task 2: Centralize message commit ownership

**Files:**
- Modify: `frontend/src/chat/DocumentChat.tsx`

- [ ] Track the active session synchronously and invalidate requests on every selection.
- [ ] Assign a monotonically increasing generation to every message request.
- [ ] Gate both `setMessages` and `setError` on matching session and newest generation.
- [ ] Route effect loads and post-send refreshes through the same guarded loader.

### Task 3: Verify and deliver

- [ ] Run the focused race tests, full Frontend tests, lint, typecheck, build, formatting/diff checks, and secret scan.
- [ ] Obtain an independent Critical/Important review and resolve every finding.
- [ ] Commit/push, verify branch and MR pipelines, document evidence, and close #286 only when green.
