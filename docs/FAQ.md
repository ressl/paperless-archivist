# FAQ

## What is Paperless Archivist?

Paperless Archivist is an AI automation layer for Paperless-ngx. It adds OCR,
tagging, metadata extraction, review queues, autopilot, document chat/RAG,
auditability, and UI-managed settings.

## Does it replace Paperless-ngx?

No. Paperless-ngx remains the system of record. Archivist reads and writes
through the Paperless REST API.

## Does Archivist write directly to the Paperless database?

No. Direct Paperless database writes are intentionally out of scope.

## Can I use local AI only?

Yes. Use the Ollama provider. The model dropdown shows installed local models
loaded through the Archivist backend.

## Can I use OpenAI, Anthropic, Ollama Cloud, or another provider?

Yes. Default provider records are included. Enter the API key, choose models,
save, and test the provider. External providers may receive document content, so
enable them only when allowed by your policy.

## What is the difference between review mode and full autopilot?

Review mode creates suggestions that a human approves or rejects. Full autopilot
selects documents and applies validated patches automatically. Invalid or risky
output can fall back to review.

## How do I turn review mode off?

Open Settings or the dashboard workflow controls and switch from manual review
to full autopilot. Confirm that validation, safety limits, dry-run, and pause
settings match your risk policy before doing this.

## What are completion tags?

Completion tags are Paperless tags Archivist adds after successful stages, for
example OCR complete or tagging complete. They make processing state visible in
Paperless and prevent unnecessary repeat work.

## Why is a saved Ollama model shown as not installed?

The saved setting still exists, but the current Ollama `/api/tags` response did
not include that model. Pull the model in Ollama or select an installed model.

## Which language controls generated tags?

`Tag output language` controls the language of newly generated business tags.
Document language detection is separate and is used as prompt context.

## Does Document Chat expose my documents to providers?

If you use local Ollama, the model call stays local. If you use an external
provider, relevant document snippets and the user question may be sent to that
provider.

## How do I know what the worker is doing?

Use the dashboard live processing panel. It shows selector, LLM, and Paperless
status, active jobs/runs, recent model events, and recent failures.

## Can I deploy it to Kubernetes?

Yes. The repository includes a generic public-safe Kustomize package under
`deploy/kubernetes`. Patch it with your image, secrets, ingress, and platform
policy.
