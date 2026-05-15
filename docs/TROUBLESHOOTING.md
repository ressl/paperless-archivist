# Troubleshooting

Use this guide when setup, sync, model calls, worker jobs, or applies do not
behave as expected.

## Paperless Test Returns 406 Not Acceptable

Most common cause: the Paperless Base URL points to Archivist or a proxy route
instead of the Paperless-ngx API.

Fix:

1. Open Settings.
2. Set Paperless Base URL to the URL reachable from the Archivist backend, for
   example `http://paperless:8000` in Compose.
3. Save settings.
4. Test again.

Also check reverse proxy routing and `Accept` header modifications.

## Paperless Test Returns 401 Or 403

The Paperless API token is missing, expired, invalid, or lacks permissions.

Fix:

- create a new Paperless API token
- enter it in Settings
- save settings
- confirm the Paperless user can read documents and update metadata/tags

## Paperless Sync Finds No Documents

Check:

- Paperless URL points to Paperless-ngx, not Archivist
- token user can see documents
- network/DNS from backend container or pod to Paperless works
- Paperless API is not hidden behind an incompatible proxy path
- delta sync cursor is not unexpectedly ahead of Paperless changes

Run a full sync after changing Paperless URL, token, or archive profile.

## Ollama Test Fails

Check:

- Ollama service is running
- Base URL is reachable from Archivist backend/worker
- local Compose uses `http://ollama:11434`
- selected model is installed
- model can run on available CPU/GPU/VRAM
- provider timeout is high enough for first model load

Use the model refresh button in Settings to reload installed models.

## Ollama Model List Is Empty

The `/api/tags` response had no installed models or Ollama was unreachable.

Fix:

```bash
ollama pull qwen3:8b
ollama pull qwen2.5vl:7b
```

Then click model refresh in Settings.

## OCR Fails With Model Runtime Error

A vision model can fail because of model/runtime incompatibility, insufficient
VRAM, bad quantization, or an upstream Ollama runner crash.

Fix:

- switch to a smaller or better-supported vision model
- reduce OCR page limit
- restart Ollama
- inspect dashboard recent failures
- run one single-document OCR job before batch processing

## Worker Does Not Process Jobs

Check:

- worker process is running
- worker uses the same `DATABASE_URL` and `ARCHIVIST_SECRET_KEY` as API
- API migrations completed
- jobs are not waiting for review
- workflow is not paused
- full autopilot is not in dry-run when you expect apply
- provider and Paperless tests pass

Use dashboard live processing status and recovery tools for stale leases or
stuck runs.

## Review Queue Keeps Filling

Possible causes:

- workflow mode requires manual review
- validation confidence is below threshold
- full autopilot fallback-to-review is enabled
- overwrite settings protect existing metadata
- model output does not match allowed tags/correspondents/document types

Fix by adjusting prompts, allowed metadata, confidence thresholds, or workflow
mode after reviewing sample output.

## Full Autopilot Does Nothing

Check:

- workflow mode is `full_auto`
- automatic processing is not paused
- dry-run is disabled if you expect Paperless writes
- hourly/daily document limits are not exhausted
- enabled stages still need work
- include/exclude tag rules allow the documents

## Completion Tags Are Missing

Check:

- the relevant stage actually succeeded
- Paperless token can update tags
- completion tag names are configured correctly
- Paperless consistency check has no metadata mismatch
- run completion-tag reconcile as dry-run before apply

## Generated Tags Use The Wrong Language

Set `Tag output language` in Settings. UI language and document language
detection do not control tag output by themselves.

## Chat Gives Weak Answers

Check:

- inventory sync has current document titles/tags
- OCR/text exists for the relevant documents
- document ID filter is correct
- text model is strong enough
- provider timeout is long enough

Use citations to identify which documents were used.

## Where To Look First

1. Dashboard live processing panel.
2. Recent failures on the dashboard.
3. Audit log.
4. Worker logs.
5. Paperless connection test.
6. Provider connection test.
7. Paperless consistency check.
