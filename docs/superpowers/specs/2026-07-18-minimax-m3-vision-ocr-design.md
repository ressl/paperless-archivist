# MiniMax M3 Vision and OCR Design

Status: approved by the operator on 2026-07-18

## Goal

Expose `ressl/MiniMax-M3-uncensored-NVFP4` as an optional vision/OCR model in
Paperless Archivist. Operators choose whether OCR uses MiniMax M3 through the
existing provider defaults and per-stage model override controls. The feature
must not introduce a new provider kind or automatically enable the OCR stage.

## Evidence and scope

MiniMax M3 is natively multimodal. The exact NVFP4 checkpoint retains its
vision tower and multimodal projectors in BF16 and has passed an exact OCR
probe through the pinned production SGLang runtime. The implementation promotes
the existing generic OpenAI-compatible image path from informational support to
the supported MiniMax M3 contract.

This change covers rendered page images sent by Archivist's existing OCR
pipeline. It does not add direct PDF upload, video input, a new OCR renderer,
or automatic routing based on document contents.

## Architecture

SGLang remains `openai_compatible`. The existing `VisionProvider`
implementation already sends standard OpenAI `image_url` content containing
base64 data URIs. MiniMax-specific behavior continues to be selected only by
the exact model ID, never by a substring or by the generic provider kind.

The built-in `sglang-minimax-m3` preset carries the exact M3 model as both its
text and vision default while remaining disabled until configured. The editable
model catalog contains separate text and vision entries for the same model ID.
The Settings UI renders the normal vision-model selector for this provider, so
the operator may keep M3, select another model exposed by the endpoint, or route
OCR to another provider with the existing stage override.

Persisted settings are normalized idempotently. Missing M3 text and vision
catalog entries are appended independently, existing operator-authored catalog
metadata is preserved per capability, and the exact built-in preset's legacy
null vision default is upgraded to the exact M3 model. Global defaults, stage
overrides, provider enabled state, and workflow enabled stages are not changed.

## Request and response contract

`VisionRequest` gains the resolved provider reasoning effort. The worker copies
the OCR stage provider's effective value into every page request. For the exact
M3 model, the OpenAI-compatible vision payload requires that value and maps it
identically to text requests:

| Archivist effort | `chat_template_kwargs.thinking_mode` |
| --- | --- |
| `off` | `disabled` |
| `low` | `adaptive` |
| `medium` or `high` | `enabled` |

Other OpenAI-compatible vision models receive no MiniMax-only field. Ollama,
Anthropic, and MinerU retain their current wire behavior. Response extraction
continues to keep separated reasoning out of OCR text and rejects a
reasoning-only response.

## Validation and release gates

Automated tests must prove:

1. the backend preset and normalized catalog expose M3 for vision without
   duplicating entries or replacing custom metadata;
2. the frontend vision picker is enabled for the M3 preset and contains the
   exact model;
3. M3 vision payloads send the correct `thinking_mode`, reject unresolved
   tuning, retain `image_url` data, and do not leak MiniMax fields to other
   models;
4. the worker carries resolved OCR-stage reasoning into `VisionRequest`;
5. the opt-in live contract treats the synthetic vision probe as a release
   gate and includes a deterministic embedded-image OCR probe;
6. Rust tests, frontend tests, formatting, type checking, lint contracts, and
   production builds pass.

Documentation must distinguish model capability from operator selection and
describe both ways to choose M3 for OCR: as the selected provider's vision
default or as the explicit OCR stage override.

## Compatibility and rollback

No database or OpenAPI schema migration is required because the provider and
catalog shapes already represent vision models. Rollback restores the prior
binary and UI; persisted vision catalog entries and the provider's model string
remain valid generic OpenAI-compatible settings and can be changed by the
operator.
