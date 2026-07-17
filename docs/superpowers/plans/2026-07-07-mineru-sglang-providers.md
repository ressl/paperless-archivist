# MinerU Provider Kind + OpenAI-Compatible Reasoning Hardening — Implementation Plan

> **Historical implementation record (superseded 2026-07-17):** Do not use
> the MiniMax M2.7 names, example values, commands, or unchecked steps below as
> an operator runbook. ADR-014 replaced the target with the text-only
> `ressl/MiniMax-M3-uncensored-NVFP4` contract. Use the current
> [Settings guide](../../USER_GUIDE.md#sglang-with-minimax-m3-text-only) and
> [operations runbook](../../OPERATIONS.md#sglangminimax-m3-operations).
> The remaining text is preserved unchanged as the implementation history for
> generic OpenAI-compatible hardening and the MinerU provider.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make SGLang-served MiniMax-M2.7 usable for all text stages (think-block stripping, `max_output_tokens`, structured-output compatibility switch with one-shot 400 self-healing) and add a vision-only `mineru` provider kind that OCRs pages via the MinerU API server (page image → Markdown).

**Architecture:** No new kind for SGLang — the existing `openai_compatible` path is hardened in `archivist-ai` (response parsing + payload builders) and plumbed through `ProviderTuning`/`StageProvider`. MinerU gets a new `AiProviderKind::Mineru` variant with its own `MineruClient` (multipart `/file_parse`), its own health check (GET `/docs`), a static model listing, and UI affordances. Spec: `docs/superpowers/specs/2026-07-07-mineru-sglang-providers-design.md`.

**Tech Stack:** Rust workspace (crates: archivist-core, archivist-ai, archivist-worker, archivist-api), reqwest 0.13 (workspace features already include `multipart` + `form`), serde, React/TypeScript frontend (`frontend/`), OpenAPI types generated via `cd frontend && npm run generate:client`.

## Global Constraints

- No new Rust dependencies. reqwest multipart is already enabled workspace-wide (`Cargo.toml:42`).
- Code comments in English, matching existing style (explain constraints, not narration).
- Wire values: kind `mineru`; `StructuredOutputMode` = `auto` | `json_object` | `off` (snake_case serde).
- Every task ends with its crate's tests green: `cargo test -p <crate>`; final task runs `cargo test --workspace`, `cargo clippy --workspace`, `cargo fmt`, `cd frontend && npm run build`.
- Commits end with: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`. Push after each task (repo convention).
- Line numbers below are anchors as of commit `4c7c7cd`; verify with the named symbols if drifted.
- The working tree may contain an unrelated uncommitted change to `crates/archivist-ocr/src/lib.rs` — NEVER `git add` that file or `git add -A`; always stage explicit paths.

---

### Task 1: Core tuning fields (`max_output_tokens`, `StructuredOutputMode`)

**Files:**
- Modify: `crates/archivist-core/src/lib.rs` (ProviderTuning ~:1735, EffectiveTuning ~:1799, resolve_tuning ~:864, ReasoningEffort neighborhood ~:1718)
- Test: same file, existing `mod tests`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub enum StructuredOutputMode { Auto, JsonObject, Off }` (Default = Auto, Copy, snake_case serde); `ProviderTuning.max_output_tokens: Option<u32>`; `ProviderTuning.structured_output: Option<StructuredOutputMode>`; `EffectiveTuning.max_output_tokens: Option<u32>`; `EffectiveTuning.structured_output: StructuredOutputMode`.

- [ ] **Step 1: Write the failing test** — append to the core `mod tests`:

```rust
#[test]
fn tuning_resolves_max_output_tokens_and_structured_output() {
    let mut settings = RuntimeSettings::default();
    settings.ai.ensure_default_providers();
    settings.ai.default_provider = "openai-compatible".to_owned();
    let provider = settings
        .ai
        .providers
        .iter_mut()
        .find(|provider| provider.name == "openai-compatible")
        .expect("preset exists");
    provider.enabled = true;

    // Unset falls through to the defaults: no cap, Auto mode.
    let effective = settings.effective_tuning();
    assert_eq!(effective.max_output_tokens, None);
    assert_eq!(effective.structured_output, StructuredOutputMode::Auto);

    let provider = settings
        .ai
        .providers
        .iter_mut()
        .find(|provider| provider.name == "openai-compatible")
        .expect("preset exists");
    provider.tuning.max_output_tokens = Some(8192);
    provider.tuning.structured_output = Some(StructuredOutputMode::JsonObject);
    let effective = settings.effective_tuning();
    assert_eq!(effective.max_output_tokens, Some(8192));
    assert_eq!(effective.structured_output, StructuredOutputMode::JsonObject);
}

#[test]
fn structured_output_mode_serializes_snake_case() {
    assert_eq!(
        serde_json::to_string(&StructuredOutputMode::JsonObject).unwrap(),
        "\"json_object\""
    );
    assert_eq!(
        serde_json::from_str::<StructuredOutputMode>("\"off\"").unwrap(),
        StructuredOutputMode::Off
    );
}
```

If `RuntimeSettings::default()` does not exist, reuse whatever constructor the neighboring tuning tests in that module use (search `fn effective_tuning` usages in `mod tests`) — the assertions stay identical.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p archivist-core tuning_resolves_max_output_tokens 2>&1 | tail -5`
Expected: compile error — `max_output_tokens` not found / `StructuredOutputMode` not defined.

- [ ] **Step 3: Implement** — directly below the `ReasoningEffort` impl (~:1733) add:

```rust
/// Wire-level structured-output mode for OpenAI / OpenAI-compatible
/// providers. `Auto` keeps the strict `json_schema` response_format
/// (OpenAI Structured Outputs). `JsonObject` downgrades to
/// `{"type":"json_object"}` for servers whose grammar backend rejects
/// strict schemas (seen on SGLang/vLLM with some reasoning models).
/// `Off` sends no response_format and relies on prompt steering plus the
/// defensive `normalize_model_json` parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputMode {
    #[default]
    Auto,
    JsonObject,
    Off,
}
```

In `ProviderTuning` (after `reasoning_effort`, before `// --- Resource caps ---`):

```rust
    /// Output-token cap sent as `max_tokens` to OpenAI / OpenAI-compatible
    /// providers. None = omit the field (server default applies). Reasoning
    /// tokens count toward the cap on most servers, so budget generously
    /// for thinking models (e.g. 8192). Other kinds ignore it.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Structured-output wire mode for schema-carrying requests on OpenAI /
    /// OpenAI-compatible providers. None = inherit (Auto).
    #[serde(default)]
    pub structured_output: Option<StructuredOutputMode>,
```

In `EffectiveTuning` (after `reasoning_effort`): `pub max_output_tokens: Option<u32>,` and `pub structured_output: StructuredOutputMode,`. In `resolve_tuning` (after the `reasoning_effort` entry):

```rust
            max_output_tokens: tuning.and_then(|tuning| tuning.max_output_tokens),
            structured_output: tuning
                .and_then(|tuning| tuning.structured_output)
                .unwrap_or_default(),
```

Then `cargo build -p archivist-core` and fix every `EffectiveTuning { .. }` literal the compiler flags (tests in this crate) by adding the two fields (`max_output_tokens: None, structured_output: StructuredOutputMode::Auto`).

- [ ] **Step 4: Run tests**

Run: `cargo test -p archivist-core 2>&1 | tail -3`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/archivist-core/src/lib.rs
git commit -m "feat(core): max_output_tokens + structured_output tuning fields

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 2: Request plumbing + OpenAI payload (`max_tokens`, structured-output modes)

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs` (ChatRequest :193, VisionRequest :229, build_openai_chat_payload :563, OpenAiCompatibleClient::vision :874; stale doc comment :206-218)
- Modify (compiler-driven literal sites): `crates/archivist-ai/src/lib.rs` (prompt builders ~:1732-:2036, tests ~:2684-:3362), `crates/archivist-ai/tests/quota_provider_name.rs:31`, `crates/archivist-api/src/main.rs` (:1920, :2681, :2701, :4678, :8604), `crates/archivist-worker/src/main.rs:1664`
- Test: `crates/archivist-ai/src/lib.rs` `mod tests`

**Interfaces:**
- Consumes: `archivist_core::StructuredOutputMode` (Task 1).
- Produces: `ChatRequest.max_output_tokens: Option<u32>`, `ChatRequest.structured_output: Option<StructuredOutputMode>`, `VisionRequest.max_output_tokens: Option<u32>`; `build_openai_chat_payload` honors all three.

- [ ] **Step 1: Write the failing tests** — append to the ai `mod tests`, modeled on `openai_chat_payload_wraps_response_schema_in_strict_json_schema_envelope` (:2669):

```rust
#[test]
fn openai_chat_payload_sets_max_tokens_when_tuned() {
    let request = ChatRequest {
        model: "nvidia/MiniMax-M2.7-NVFP4".to_owned(),
        system_prompt: String::new(),
        user_prompt: "hi".to_owned(),
        temperature: 0.0,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: Some(8192),
        structured_output: None,
    };
    let payload = build_openai_chat_payload(&request);
    assert_eq!(payload["max_tokens"], 8192);
}

#[test]
fn openai_chat_payload_omits_max_tokens_by_default() {
    let request = ChatRequest {
        model: "gpt-4o-mini".to_owned(),
        system_prompt: String::new(),
        user_prompt: String::new(),
        temperature: 0.0,
        num_ctx: None,
        response_schema: None,
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: None,
    };
    assert!(build_openai_chat_payload(&request).get("max_tokens").is_none());
}

#[test]
fn openai_chat_payload_structured_output_json_object_mode() {
    let schema = json!({ "type": "object" });
    let request = ChatRequest {
        model: "m".to_owned(),
        system_prompt: String::new(),
        user_prompt: String::new(),
        temperature: 0.0,
        num_ctx: None,
        response_schema: Some(schema),
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: Some(StructuredOutputMode::JsonObject),
    };
    let payload = build_openai_chat_payload(&request);
    assert_eq!(payload["response_format"], json!({ "type": "json_object" }));
}

#[test]
fn openai_chat_payload_structured_output_off_mode() {
    let schema = json!({ "type": "object" });
    let request = ChatRequest {
        model: "m".to_owned(),
        system_prompt: String::new(),
        user_prompt: String::new(),
        temperature: 0.0,
        num_ctx: None,
        response_schema: Some(schema),
        reasoning_effort: None,
        max_output_tokens: None,
        structured_output: Some(StructuredOutputMode::Off),
    };
    assert!(build_openai_chat_payload(&request).get("response_format").is_none());
}
```

- [ ] **Step 2: Verify failure** — `cargo test -p archivist-ai openai_chat_payload_sets_max_tokens 2>&1 | tail -5` → compile error (missing fields).

- [ ] **Step 3: Implement**
  1. Import `StructuredOutputMode` where `ReasoningEffort` is imported from `archivist_core`.
  2. `ChatRequest`: after `reasoning_effort` add the two fields with these doc comments:

```rust
    /// Output-token cap forwarded as `max_tokens` by the OpenAI-compatible
    /// payload builder. None = omit (server default). Populated from the
    /// provider's tuning. Ollama sizes output via num_ctx and Anthropic
    /// pins max_tokens itself, so both ignore this field. Added v1.17.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Structured-output wire mode applied when `response_schema` is set;
    /// see [`StructuredOutputMode`]. None = Auto (strict json_schema).
    #[serde(default)]
    pub structured_output: Option<StructuredOutputMode>,
```

  3. `VisionRequest`: after `num_ctx` add `max_output_tokens: Option<u32>` with a one-line doc (`/// See [`ChatRequest::max_output_tokens`]. Added v1.17.`) and `#[serde(default)]`.
  4. `build_openai_chat_payload`: after the reasoning-effort block insert:

```rust
    if let Some(max_tokens) = request.max_output_tokens {
        payload
            .as_object_mut()
            .expect("payload is an object literal")
            .insert("max_tokens".to_owned(), json!(max_tokens));
    }
```

  and wrap the existing schema block in the mode match (keep the existing json_schema envelope code verbatim inside the `Auto` arm):

```rust
    if let Some(schema) = request.response_schema.as_ref() {
        match request.structured_output.unwrap_or_default() {
            StructuredOutputMode::Auto => {
                // ... existing response_format json_schema insert, unchanged ...
            }
            StructuredOutputMode::JsonObject => {
                // Plain JSON mode: shape is grammar-free, vocabularies are
                // enforced only by the prompt + defensive parser. For
                // servers whose grammar backend chokes on strict schemas.
                payload
                    .as_object_mut()
                    .expect("payload is an object literal")
                    .insert("response_format".to_owned(), json!({ "type": "json_object" }));
            }
            StructuredOutputMode::Off => {}
        }
    }
```

  5. `OpenAiCompatibleClient::vision` (:874): inside the `spawn_blocking` closure, bind the `json!({...})` to `let mut payload = ...;`, then before returning it:

```rust
            if let Some(max_tokens) = request.max_output_tokens {
                payload
                    .as_object_mut()
                    .expect("payload is an object literal")
                    .insert("max_tokens".to_owned(), json!(max_tokens));
            }
            payload
```

  6. Fix the stale `response_schema` doc comment on `ChatRequest` (:206-218): replace the sentence "The OpenAI-compatible and Anthropic clients ignore this field for now — adding `response_format: json_schema` and tool-use respectively is tracked as separate work." with "The OpenAI-compatible client forwards it as `response_format: json_schema` (mode-dependent, see `StructuredOutputMode`) and the Anthropic client as forced tool-use."
  7. `cargo build --workspace 2>&1 | grep -E "^error" -A 3` — add `max_output_tokens: None, structured_output: None,` (chat) / `max_output_tokens: None,` (vision) to every literal the compiler flags (files listed in **Files** above).

- [ ] **Step 4: Run tests** — `cargo test -p archivist-ai 2>&1 | tail -3` and `cargo build --workspace` → PASS / clean.

- [ ] **Step 5: Commit**

```bash
git add crates/archivist-ai crates/archivist-api/src/main.rs crates/archivist-worker/src/main.rs
git commit -m "feat(ai): max_tokens + structured-output modes for OpenAI-compatible payloads

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 3: Think-block stripping + reasoning_content guard

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs` (new helpers near `extract_model_ids` :944; chat text extraction :854-:862; vision text extraction :922-:930)
- Test: same file `mod tests`

**Interfaces:**
- Consumes: nothing new.
- Produces: `fn strip_think_blocks(text: &str) -> String`; `fn extract_openai_message_text(raw: &Value) -> Result<String>` — used by both TextProvider::chat and VisionProvider::vision of `OpenAiCompatibleClient` (and by Task 4's retry path).

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn strip_think_blocks_removes_single_and_multiple_blocks() {
    assert_eq!(
        strip_think_blocks("<think>plan</think>{\"title\":\"A\"}"),
        "{\"title\":\"A\"}"
    );
    assert_eq!(
        strip_think_blocks("a<think>x</think>b<think>y</think>c"),
        "abc"
    );
}

#[test]
fn strip_think_blocks_drops_unterminated_tail() {
    assert_eq!(strip_think_blocks("answer<think>never closed"), "answer");
}

#[test]
fn extract_openai_message_text_strips_think_and_trims() {
    let raw = json!({ "choices": [{ "message": {
        "content": "<think>reasoning...</think>\n  final answer  " } }] });
    assert_eq!(extract_openai_message_text(&raw).unwrap(), "final answer");
}

#[test]
fn extract_openai_message_text_errors_on_reasoning_only_response() {
    // SGLang --reasoning-parser puts thinking into reasoning_content; if the
    // model never produced a final answer the content is empty. That must be
    // a loud error, not silent empty text.
    let raw = json!({ "choices": [{ "message": {
        "content": "", "reasoning_content": "thinking forever" } }] });
    let error = extract_openai_message_text(&raw).unwrap_err().to_string();
    assert!(error.contains("reasoning"), "got: {error}");

    let raw = json!({ "choices": [{ "message": { "content": "<think>only thoughts</think>" } }] });
    assert!(extract_openai_message_text(&raw).is_err());
}

#[test]
fn extract_openai_message_text_keeps_legitimate_empty_content() {
    // A blank OCR page can legitimately come back empty — that is not an
    // error (document-level min_chars validation handles it downstream).
    let raw = json!({ "choices": [{ "message": { "content": "" } }] });
    assert_eq!(extract_openai_message_text(&raw).unwrap(), "");
}

#[test]
fn extract_openai_message_text_ignores_reasoning_alongside_answer() {
    let raw = json!({ "choices": [{ "message": {
        "content": "answer", "reasoning_content": "thoughts" } }] });
    assert_eq!(extract_openai_message_text(&raw).unwrap(), "answer");
}
```

- [ ] **Step 2: Verify failure** — `cargo test -p archivist-ai strip_think 2>&1 | tail -5` → compile error.

- [ ] **Step 3: Implement** — above `extract_model_ids` add:

```rust
/// Removes `<think>...</think>` reasoning blocks that thinking models
/// (MiniMax-M2, DeepSeek-R1, QwQ) emit inline when the serving layer does
/// not split them into `reasoning_content`. An unterminated `<think>`
/// drops everything from the tag to the end — the model never surfaced a
/// final answer after it.
fn strip_think_blocks(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        output.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(end_rel) => rest = &rest[start + end_rel + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    output.push_str(rest);
    output
}

/// Extracts the assistant text from an OpenAI-style chat/vision response,
/// tolerating reasoning models: inline `<think>` blocks are stripped and a
/// response whose visible content is empty while reasoning text exists is
/// rejected with a configuration hint instead of silently returning "".
/// Plain empty content (blank OCR page) stays valid.
fn extract_openai_message_text(raw: &Value) -> Result<String> {
    let message = raw
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"));
    let content = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let stripped = strip_think_blocks(content);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        let had_think = content.contains("<think>");
        let reasoning_only = message
            .and_then(|message| message.get("reasoning_content"))
            .and_then(Value::as_str)
            .is_some_and(|reasoning| !reasoning.trim().is_empty());
        if had_think || reasoning_only {
            return Err(anyhow!(
                "model returned only reasoning content and no final answer — \
                 check the server's reasoning-parser configuration"
            ));
        }
        return Ok(String::new());
    }
    Ok(trimmed.to_owned())
}
```

Replace BOTH extraction blocks (`let text = raw.get("choices")...to_owned();` in chat :854-:862 and vision :922-:930) with `let text = extract_openai_message_text(&raw)?;`.

- [ ] **Step 4: Run tests** — `cargo test -p archivist-ai 2>&1 | tail -3` → PASS (existing chat/vision tests must stay green; none asserts think-passthrough).

- [ ] **Step 5: Commit**

```bash
git add crates/archivist-ai/src/lib.rs
git commit -m "feat(ai): strip think blocks and guard reasoning-only responses

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 4: One-shot retry without response_format on schema 400s

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs` (`OpenAiCompatibleClient::chat` :832)
- Test: same file `mod tests`

**Interfaces:**
- Consumes: `extract_openai_message_text` (Task 3), `StructuredOutputMode` (Task 1).
- Produces: `fn should_retry_without_schema(status: u16, body: &str, retryable_request: bool) -> bool`.

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn should_retry_without_schema_matrix() {
    let body = "Bad Request: response_format json_schema is not supported by the xgrammar backend";
    assert!(should_retry_without_schema(400, body, true));
    assert!(!should_retry_without_schema(400, body, false)); // no schema was sent
    assert!(!should_retry_without_schema(500, body, true)); // only 400s
    assert!(!should_retry_without_schema(
        400,
        "model not found", // unrelated 400
        true
    ));
    assert!(should_retry_without_schema(400, "invalid GRAMMAR compilation", true));
}
```

- [ ] **Step 2: Verify failure** — `cargo test -p archivist-ai should_retry_without_schema 2>&1 | tail -5` → compile error.

- [ ] **Step 3: Implement** — helper next to `extract_openai_message_text`:

```rust
/// True when a 400 looks like the server rejecting our `response_format`
/// (strict json_schema unsupported by the grammar backend — observed on
/// SGLang/vLLM with some reasoning models). One retry without the field
/// keeps the pipeline alive; `normalize_model_json` still guards shape.
fn should_retry_without_schema(status: u16, body: &str, retryable_request: bool) -> bool {
    if status != 400 || !retryable_request {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    ["response_format", "json_schema", "grammar", "schema"]
        .iter()
        .any(|needle| lowered.contains(needle))
}
```

Rework `TextProvider::chat` for `OpenAiCompatibleClient` (keep everything else identical):

```rust
    async fn chat(&self, request: ChatRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let mut payload = build_openai_chat_payload(&request);
        // Self-healing is only armed in Auto mode: JsonObject/Off are already
        // the operator's explicit compatibility choice.
        let retryable = payload.get("response_format").is_some()
            && request.structured_output.unwrap_or_default() == StructuredOutputMode::Auto;
        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .context("call OpenAI-compatible chat")?;
        let status = response.status();
        let raw: Value = if status.is_success() {
            response.json().await.context("decode OpenAI-compatible response")?
        } else {
            let (status, body) = check_quota_then_take_body(&self.provider_name, response).await?;
            if !should_retry_without_schema(status.as_u16(), &body, retryable) {
                return Err(
                    anyhow::Error::new(AiProviderError::from_http(status.as_u16(), body))
                        .context("OpenAI-compatible chat call"),
                );
            }
            tracing::warn!(
                provider = %self.provider_name,
                "server rejected response_format (400); retrying once without structured output"
            );
            payload
                .as_object_mut()
                .expect("payload is an object literal")
                .remove("response_format");
            let retry = self
                .client
                .post(&url)
                .json(&payload)
                .send()
                .await
                .context("call OpenAI-compatible chat (schema retry)")?;
            let retry_status = retry.status();
            if !retry_status.is_success() {
                let (retry_status, retry_body) =
                    check_quota_then_take_body(&self.provider_name, retry).await?;
                return Err(anyhow::Error::new(AiProviderError::from_http(
                    retry_status.as_u16(),
                    retry_body,
                ))
                .context("OpenAI-compatible chat call (schema retry)"));
            }
            retry
                .json()
                .await
                .context("decode OpenAI-compatible response (schema retry)")?
        };
        let text = extract_openai_message_text(&raw)?;
        Ok(AiResponse {
            provider: self.provider_name.clone(),
            model: request.model,
            text,
            raw_response: raw,
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        })
    }
```

If `tracing` is not already a dependency of archivist-ai (`grep tracing crates/archivist-ai/Cargo.toml`), add `tracing.workspace = true` to its `[dependencies]`.

- [ ] **Step 4: Run tests** — `cargo test -p archivist-ai 2>&1 | tail -3` → PASS (including `crates/archivist-ai/tests/quota_provider_name.rs`, which exercises the non-retry error path).

- [ ] **Step 5: Commit**

```bash
git add crates/archivist-ai
git commit -m "feat(ai): one-shot retry without response_format on schema 400s

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 5: Worker plumbing (StageProvider carries the new tuning)

**Files:**
- Modify: `crates/archivist-worker/src/main.rs` (StageProvider :4110, provider_for_stage :4121, chat_for_stage :4229, OCR VisionRequest literal :1664, stale schema comment ~:2213)
- Test: same file `mod tests`

**Interfaces:**
- Consumes: Task 1 core fields, Task 2 request fields.
- Produces: `StageProvider.max_output_tokens: Option<u32>`, `StageProvider.structured_output: StructuredOutputMode` — populated for every stage call (metadata, classifier, consensus all route through `chat_for_stage`; OCR through the VisionRequest literal).

- [ ] **Step 1: Write the failing test** — append to the worker `mod tests` (reuse the settings-construction style of the neighboring `provider_for_stage`/`stage_provider` tests around :4484):

```rust
#[test]
fn provider_for_stage_carries_max_output_tokens_and_structured_output() {
    let mut settings = RuntimeSettings::default();
    settings.ai.ensure_default_providers();
    settings.ai.default_provider = "openai-compatible".to_owned();
    let provider = settings
        .ai
        .providers
        .iter_mut()
        .find(|provider| provider.name == "openai-compatible")
        .expect("preset exists");
    provider.enabled = true;
    provider.tuning.max_output_tokens = Some(8192);
    provider.tuning.structured_output = Some(StructuredOutputMode::JsonObject);

    let resolved = provider_for_stage(&settings, Stage::Metadata, false).expect("provider resolves");
    assert_eq!(resolved.max_output_tokens, Some(8192));
    assert_eq!(resolved.structured_output, StructuredOutputMode::JsonObject);
}
```

Add `StructuredOutputMode` to the existing `archivist_core` imports at the top of main.rs.

- [ ] **Step 2: Verify failure** — `cargo test -p archivist-worker provider_for_stage_carries 2>&1 | tail -5` → compile error.

- [ ] **Step 3: Implement**
  1. `StageProvider` struct: add `max_output_tokens: Option<u32>,` and `structured_output: StructuredOutputMode,` after `reasoning_effort`.
  2. `provider_for_stage`: before the final `Ok(StageProvider {...})`:

```rust
    let max_output_tokens = provider.tuning.max_output_tokens.filter(|tokens| *tokens > 0);
    let structured_output = provider.tuning.structured_output.unwrap_or_default();
```

  and the two fields in the literal.
  3. `chat_for_stage`: after `request.reasoning_effort = Some(provider.reasoning_effort);` add:

```rust
    request.max_output_tokens = provider.max_output_tokens;
    request.structured_output = Some(provider.structured_output);
```

  4. OCR `VisionRequest` literal (:1664): change the Task-2 placeholder `max_output_tokens: None,` to `max_output_tokens: provider.max_output_tokens,`.
  5. Fix the stale comment block above `request.response_schema = ...` (~:2213-:2219): replace "OpenAI-compatible and Anthropic clients ignore this field today; their wire-level enforcement (response_format json_schema / tool-use) is tracked as future work." with "The OpenAI-compatible client forwards it as `response_format` (json_schema/json_object per the provider's `structured_output` tuning, with a one-shot schema-400 fallback), and the Anthropic client as forced tool-use."

- [ ] **Step 4: Run tests** — `cargo test -p archivist-worker 2>&1 | tail -3` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/archivist-worker/src/main.rs
git commit -m "feat(worker): plumb max_output_tokens/structured_output into stage requests

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 6: MineruClient (archivist-ai)

**Files:**
- Modify: `crates/archivist-ai/src/lib.rs` (new client after `OpenAiCompatibleClient`'s impls, ~:940)
- Test: same file `mod tests`

**Interfaces:**
- Consumes: existing `VisionProvider` trait, `AiResponse`, `VisionRequest`, `AiProviderError`, `SecretString`.
- Produces: `pub struct MineruClient` with `new(provider_name, base_url, api_key) -> Result<Self>`, `new_with_timeout(..., timeout: Duration) -> Result<Self>`, `pub async fn test_connection(&self) -> Result<Value>`; `impl VisionProvider for MineruClient`; `fn extract_mineru_markdown(raw: &Value) -> Option<String>`.

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn mineru_markdown_extraction_handles_known_response_shapes() {
    // Single-file shape.
    let raw = json!({ "md_content": "# Rechnung\n\nBetrag: 42,00 EUR" });
    assert_eq!(
        extract_mineru_markdown(&raw).as_deref(),
        Some("# Rechnung\n\nBetrag: 42,00 EUR")
    );
    // Batch shape keyed by input file name (mineru web_api).
    let raw = json!({ "results": { "page.png": { "md_content": "Seite eins" } } });
    assert_eq!(extract_mineru_markdown(&raw).as_deref(), Some("Seite eins"));
    // Unknown shape -> None (caller raises a descriptive error).
    assert_eq!(extract_mineru_markdown(&json!({ "status": "ok" })), None);
}
```

- [ ] **Step 2: Verify failure** — `cargo test -p archivist-ai mineru_markdown 2>&1 | tail -5` → compile error.

- [ ] **Step 3: Implement** — after the `VisionProvider for OpenAiCompatibleClient` impl add:

```rust
/// Client for the MinerU API server (FastAPI in front of a vLLM-served
/// MinerU VLM). Protocol: POST one rendered page image to `/file_parse`
/// as multipart, receive parsed-document JSON; the OCR text is the
/// returned Markdown. Vision-only — MinerU exposes no chat endpoint, and
/// prompt/temperature/num_ctx have no meaning for its internal
/// layout+recognition pipeline.
#[derive(Clone)]
pub struct MineruClient {
    base_url: String,
    client: reqwest::Client,
    provider_name: String,
}

impl MineruClient {
    pub fn new(provider_name: &str, base_url: &str, api_key: Option<SecretString>) -> Result<Self> {
        Self::new_with_timeout(provider_name, base_url, api_key, Duration::from_secs(180))
    }

    pub fn new_with_timeout(
        provider_name: &str,
        base_url: &str,
        api_key: Option<SecretString>,
        timeout: Duration,
    ) -> Result<Self> {
        let mut headers = HeaderMap::new();
        if let Some(api_key) = api_key {
            let value = format!("Bearer {}", api_key.expose_secret());
            headers.insert(AUTHORIZATION, HeaderValue::from_str(&value)?);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client,
            provider_name: provider_name.to_owned(),
        })
    }

    /// Health probe for the provider test button. The FastAPI server serves
    /// its interactive docs at `/docs`; any 2xx proves the service is up
    /// without spending GPU time on a real parse.
    pub async fn test_connection(&self) -> Result<Value> {
        let response = self
            .client
            .get(format!("{}/docs", self.base_url))
            .send()
            .await
            .context("call MinerU /docs")?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("MinerU health check returned {status}"));
        }
        Ok(json!({ "provider": self.provider_name, "status": "ok" }))
    }
}

/// Extracts the Markdown payload from a MinerU `/file_parse` response.
/// Known shapes (fixture-pinned; re-verify when bumping the deployed
/// MinerU version): `{"md_content": "..."}` for single-file responses and
/// `{"results": {"<input-name>": {"md_content": "..."}}}` for the batch
/// envelope of mineru's web_api. Returns None on anything else so the
/// caller can error with a body snippet.
fn extract_mineru_markdown(raw: &Value) -> Option<String> {
    if let Some(md) = raw.get("md_content").and_then(Value::as_str) {
        return Some(md.to_owned());
    }
    raw.get("results")
        .and_then(Value::as_object)
        .and_then(|results| results.values().next())
        .and_then(|entry| entry.get("md_content"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[async_trait]
impl VisionProvider for MineruClient {
    async fn vision(&self, request: VisionRequest) -> Result<AiResponse> {
        let started = Instant::now();
        let Some(image) = request.images.first() else {
            return Err(anyhow!("MinerU vision call requires one page image"));
        };
        let file_name = match image.mime_type.as_str() {
            "image/jpeg" => "page.jpg",
            _ => "page.png",
        };
        let part = reqwest::multipart::Part::bytes(image.bytes.clone())
            .file_name(file_name)
            .mime_str(&image.mime_type)
            .context("build MinerU multipart part")?;
        let form = reqwest::multipart::Form::new()
            .part("files", part)
            .text("return_md", "true");
        let response = self
            .client
            .post(format!("{}/file_parse", self.base_url))
            .multipart(form)
            .send()
            .await
            .context("call MinerU file_parse")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(600).collect();
            return Err(anyhow::Error::new(AiProviderError::from_http(
                status.as_u16(),
                snippet,
            ))
            .context("MinerU file_parse call"));
        }
        let raw: Value = response.json().await.context("decode MinerU response")?;
        let Some(text) = extract_mineru_markdown(&raw) else {
            let snippet: String = raw.to_string().chars().take(600).collect();
            return Err(anyhow!("MinerU response had no md_content field: {snippet}"));
        };
        Ok(AiResponse {
            provider: self.provider_name.clone(),
            model: request.model,
            text,
            raw_response: raw,
            duration_ms: started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        })
    }
}
```

**Contract-verification note (from spec §4.2):** the multipart field names (`files`, `return_md`) and response shapes were taken from mineru's web_api; when the operator's MinerU instance is available, run `curl -F "files=@page.png" -F "return_md=true" http://<host>:<port>/file_parse | head -c 600`, compare with `extract_mineru_markdown`, and update helper + fixture test if the deployed version differs. Record the pinned MinerU version in the commit message of that adjustment.

- [ ] **Step 4: Run tests** — `cargo test -p archivist-ai 2>&1 | tail -3` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/archivist-ai/src/lib.rs
git commit -m "feat(ai): MineruClient — multipart file_parse vision provider

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 7: `AiProviderKind::Mineru` — enum variant + every backend arm

**Files:**
- Modify: `crates/archivist-core/src/lib.rs` (:1979 enum, :1823 default_providers, model catalog list ~:1326-:1602)
- Modify: `crates/archivist-worker/src/main.rs` (provider_base_url :4173, VisionClient :4347, build_vision_client :4363, chat_with_provider :4296)
- Modify: `crates/archivist-api/src/main.rs` (provider_base_url :2644, discover_provider_models :2267, test_ai_provider :2664, chat_with_default_provider :2718)
- Test: core + worker `mod tests`

**Interfaces:**
- Consumes: `MineruClient` (Task 6).
- Produces: `AiProviderKind::Mineru` (wire `mineru`); `AiProviderSettings::mineru_default()`; every Rust `match provider.kind` handles Mineru. Vision-only error string: `AI provider '<name>' uses kind "mineru" which is vision-only (OCR); select a text-capable provider for this stage`.

- [ ] **Step 1: Write the failing tests** — core `mod tests`:

```rust
#[test]
fn mineru_kind_serializes_snake_case_and_ships_disabled_preset() {
    assert_eq!(
        serde_json::to_string(&AiProviderKind::Mineru).unwrap(),
        "\"mineru\""
    );
    let presets = AiProviderSettings::default_providers();
    let mineru = presets
        .iter()
        .find(|provider| provider.name == "mineru")
        .expect("mineru preset present");
    assert_eq!(mineru.kind, AiProviderKind::Mineru);
    assert!(!mineru.enabled);
    assert_eq!(mineru.default_vision_model.as_deref(), Some("mineru"));
    assert_eq!(mineru.default_text_model, None);
}
```

worker `mod tests`:

```rust
#[test]
fn provider_base_url_defaults_for_mineru() {
    assert_eq!(
        provider_base_url(&AiProviderKind::Mineru, ""),
        "http://localhost:8001"
    );
    assert_eq!(
        provider_base_url(&AiProviderKind::Mineru, "http://omega:8001/"),
        "http://omega:8001"
    );
}
```

- [ ] **Step 2: Verify failure** — `cargo test -p archivist-core mineru_kind 2>&1 | tail -5` → compile error (`Mineru` not defined).

- [ ] **Step 3: Implement** (compiler-driven; `cargo build --workspace` after each sub-step to surface the next non-exhaustive match):
  1. core enum: add `Mineru,` after `OpenaiCompatible`.
  2. core `default_providers()`: append `Self::mineru_default(),` and add:

```rust
    pub fn mineru_default() -> Self {
        // MinerU API server (FastAPI in front of vLLM). Vision-only: the
        // pipeline may select it exclusively for the OCR stage. Disabled by
        // default like the openai-compatible preset — operators point
        // base_url at their deployment and enable it.
        Self {
            name: "mineru".to_owned(),
            kind: AiProviderKind::Mineru,
            base_url: "http://localhost:8001".to_owned(),
            default_text_model: None,
            default_vision_model: Some("mineru".to_owned()),
            cost_per_1m_input_tokens_usd: Some(0.0),
            cost_per_1m_output_tokens_usd: Some(0.0),
            secret_id: None,
            enabled: false,
            tuning: ProviderTuning::default(),
        }
    }
```

  3. NO model-catalog entry for mineru — the UI renders a fixed, disabled vision-model field for this kind (Task 9), so a picker catalog entry would be dead data. If the `use AiProviderKind::{...}` import at core :1326 or any non-exhaustive catalog match breaks the build, add `Mineru` to the import only.
  4. worker `provider_base_url`: arm `AiProviderKind::Mineru => "http://localhost:8001".to_owned(),`.
  5. worker `VisionClient`: add variant `Mineru(MineruClient),` + dispatch arm `VisionClient::Mineru(client) => client.vision(request).await,`; import `MineruClient` alongside the other archivist_ai client imports.
  6. worker `build_vision_client`: arm

```rust
        AiProviderKind::Mineru => Ok(VisionClient::Mineru(MineruClient::new_with_timeout(
            &provider.name,
            &provider.base_url,
            provider_secret(pool, config, provider).await?,
            timeout,
        )?)),
```

  7. worker `chat_with_provider`: arm

```rust
        AiProviderKind::Mineru => Err(anyhow!(
            "AI provider '{}' uses kind \"mineru\" which is vision-only (OCR); \
             select a text-capable provider for this stage",
            provider.name
        )),
```

  8. api `provider_base_url`: same arm as worker's.
  9. api `discover_provider_models`: arm

```rust
        AiProviderKind::Mineru => {
            // MinerU serves one implicit model; there is no /models endpoint.
            Ok(vec![OllamaInstalledModel::from_id("mineru".to_owned())])
        }
```

  10. api `test_ai_provider`: arm

```rust
        AiProviderKind::Mineru => {
            let client = MineruClient::new(
                &provider.name,
                &provider.base_url,
                provider_secret(state, provider).await?,
            )?;
            client.test_connection().await
        }
```

  (import `MineruClient` in api's archivist_ai imports.)
  11. api `chat_with_default_provider`: same vision-only `Err` arm as worker's (step 7 wording).
  12. `cargo build --workspace` until zero non-exhaustive-match errors; give any further flagged match (e.g. frontend-unrelated helpers like `is_ollama_cloud` guards use `if`, not `match` — no action) the vision-only or passthrough treatment that its siblings for `OpenaiCompatible` get, preferring explicit arms over `_`.

- [ ] **Step 4: Run tests** — `cargo test --workspace 2>&1 | tail -4` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/archivist-core crates/archivist-worker crates/archivist-api
git commit -m "feat: mineru provider kind — vision-only OCR via MinerU API server

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 8: OpenAPI + TypeScript types compile

**Files:**
- Modify: `openapi/openapi.yaml:31` (kind enum)
- Generate: `frontend/src/api/schema.ts` (`cd frontend && npm run generate:client`)
- Modify: `frontend/src/api/client.ts` (:56 kind union, :30 ProviderTuning, :12 near ReasoningEffort)
- Modify: `frontend/src/settings/sections/tuning.tsx` (:23 TuningPresetKind, :25 TUNING_PRESETS, :161 tuningPresetKindFor)
- Modify: `frontend/src/settings/sections/ModelCatalogEditor.tsx:6` (kind list)

**Interfaces:**
- Consumes: backend wire values from Tasks 1+7.
- Produces: TS `AiProviderKind = 'ollama' | 'openai' | 'anthropic' | 'openai_compatible' | 'mineru'`; `StructuredOutputMode = 'auto' | 'json_object' | 'off'`; `ProviderTuning.max_output_tokens?: number | null` and `.structured_output?: StructuredOutputMode | null`; `TUNING_PRESETS.mineru`.

- [ ] **Step 1: openapi.yaml** — change line 31 to `enum: [ollama, openai, anthropic, openai_compatible, mineru]`, then `cd frontend && npm run generate:client` and confirm `git diff frontend/src/api/schema.ts` shows the new enum member.
- [ ] **Step 2: client.ts** — extend the union at :56 with `| 'mineru'`; below `ReasoningEffort` (:12) add `export type StructuredOutputMode = 'auto' | 'json_object' | 'off';`; in `ProviderTuning` after `reasoning_effort` add:

```ts
  max_output_tokens?: number | null;
  structured_output?: StructuredOutputMode | null;
```

- [ ] **Step 3: tuning.tsx presets** — extend `TuningPresetKind` to `'ollama' | 'ollama_cloud' | 'openai' | 'anthropic' | 'openai_compatible' | 'mineru'`. Add `max_output_tokens: null, structured_output: null` to EVERY existing preset object (the comment at :19 demands every field listed), and append:

```ts
  mineru: {
    worker_concurrency: null,
    consensus_secondary_text_model: null,
    consensus_date_tolerance_days: null,
    text_num_ctx: null,
    vision_num_ctx: null,
    max_output_tokens: null,
    structured_output: null,
    ocr_page_limit: null,
    hourly_document_limit: null,
    daily_document_limit: null,
    metadata_confidence_threshold: null,
    title_confidence_threshold: null,
    correspondent_confidence_threshold: null,
    document_type_confidence_threshold: null,
    document_date_confidence_threshold: null,
    tags_confidence_threshold: null,
    fields_confidence_threshold: null,
    max_tags: null,
    allowed_list_max: null,
    request_timeout_seconds: null
  }
```

(Match the exact key order of the existing preset objects, including `reasoning_effort` if the existing presets carry it — mirror `anthropic`.) `tuningPresetKindFor` (:161) needs no change — `provider.kind` now includes `'mineru'` which the record covers.
- [ ] **Step 4: ModelCatalogEditor.tsx:6** — add `'mineru'` to the kind list so catalog entries can target it.
- [ ] **Step 5: Verify** — `cd frontend && npx tsc --noEmit` (or `npm run build`) → clean.
- [ ] **Step 6: Commit**

```bash
git add openapi/openapi.yaml frontend/src/api/schema.ts frontend/src/api/client.ts frontend/src/settings/sections/tuning.tsx frontend/src/settings/sections/ModelCatalogEditor.tsx
git commit -m "feat(frontend): mineru kind + new tuning fields in API types

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 9: Settings UI (ProviderCard, tuning fields, i18n, model recommendation)

**Files:**
- Modify: `frontend/src/settings/sections/ProviderCard.tsx` (kind select :65-:68, model pickers :98-:121)
- Modify: `frontend/src/settings/sections/tuning.tsx` (PERFORMANCE_FIELDS :133, TuningDisclosure Performance section :201-:249, options consts near REASONING_EFFORT_OPTIONS)
- Modify: `frontend/src/modelCatalog.ts` (`recommendedModel` :257)
- Modify: both locale files under `frontend/src/i18n/locales/` (inspect with `ls frontend/src/i18n/locales/`; add identical keys to each)

**Interfaces:**
- Consumes: types from Task 8.
- Produces: UI — kind option "mineru"; for mineru: no text-model picker, fixed vision model, no sync button; two new tuning inputs shown only for `openai`/`openai_compatible`.

- [ ] **Step 1: ProviderCard kind option** — add `<option value="mineru">mineru (OCR)</option>` after the `openai_compatible` option.
- [ ] **Step 2: ProviderCard model pickers** — wrap the text-model block (:98-:109) in `{provider.kind !== 'mineru' && ( ... )}`. Replace the vision-model block (:110-:121) content for mineru:

```tsx
      <div className="settings-field">
        {t('settings.provider.vision_model')}
        {provider.kind === 'mineru' ? (
          <input value="mineru" disabled aria-label={t('settings.provider.vision_model')} />
        ) : (
          <ProviderModelSelect
            capability="vision"
            provider={provider}
            value={provider.default_vision_model ?? ''}
            catalog={catalog}
            ollamaState={ollamaState}
            onChange={(value) => onChangeProvider({ default_vision_model: value })}
            onRefresh={onRefreshModels}
          />
        )}
      </div>
```

  Under the base-URL field add a hint for mineru: `{provider.kind === 'mineru' && <p className="muted">{t('settings.provider.mineru_base_url_hint')}</p>}` (reuse the repo's existing muted/hint class — check how other field hints in this file/`tuning.tsx` render and match it).
- [ ] **Step 3: tuning.tsx new fields** — next to `REASONING_EFFORT_OPTIONS` (search it; ~:300+) add, mirroring its exact array shape:

```ts
const STRUCTURED_OUTPUT_OPTIONS = [
  { value: 'auto', labelKey: 'settings.tuning.structured_output.auto' },
  { value: 'json_object', labelKey: 'settings.tuning.structured_output.json_object' },
  { value: 'off', labelKey: 'settings.tuning.structured_output.off' }
] as const;
```

  (If `REASONING_EFFORT_OPTIONS` uses a different property shape — e.g. plain values with labels derived from the field key — mirror that shape instead; the three values and their display intent stay the same.) Extend `PERFORMANCE_FIELDS` with `'max_output_tokens', 'structured_output'`. In the Performance `TuningSection`, after the `reasoning_effort` select, add — visible only for OpenAI-style kinds:

```tsx
          {(provider.kind === 'openai' || provider.kind === 'openai_compatible') && (
            <>
              <TuningNumberField
                field="max_output_tokens"
                value={tuning.max_output_tokens}
                defaultValue={null}
                defaultLabel={t('settings.tuning.default.max_output_tokens')}
                min={1}
                step={256}
                onChange={(value) => onChangeTuning({ max_output_tokens: value })}
              />
              <TuningSelectField
                field="structured_output"
                value={tuning.structured_output}
                options={STRUCTURED_OUTPUT_OPTIONS}
                defaultLabel={t('settings.tuning.default.structured_output')}
                onChange={(value) => onChangeTuning({ structured_output: value })}
              />
            </>
          )}
```

  (Match `TuningNumberField`/`TuningSelectField` prop signatures exactly as used by the neighboring fields — adjust prop names to what the components actually take.)
- [ ] **Step 4: i18n** — `ls frontend/src/i18n/locales/` and add to BOTH locale files (en / de values):
  - `settings.tuning.field.max_output_tokens`: "Max output tokens (max_tokens)" / "Max. Ausgabe-Tokens (max_tokens)" — key naming: follow how existing tuning field labels are keyed (search `text_num_ctx` in the locale files and mirror the pattern).
  - `settings.tuning.default.max_output_tokens`: "Server default. Reasoning tokens count toward the cap — budget generously for thinking models (e.g. 8192)." / "Server-Standard. Reasoning-Tokens zaehlen mit — fuer Denk-Modelle grosszuegig waehlen (z. B. 8192)."
  - `settings.tuning.field.structured_output`: "Structured output" / "Structured Output"
  - `settings.tuning.default.structured_output`: "Auto (strict JSON schema)" / "Auto (striktes JSON-Schema)"
  - `settings.tuning.structured_output.auto`: "auto (json_schema strict)" (both)
  - `settings.tuning.structured_output.json_object`: "json_object (schema-free JSON mode)" / "json_object (JSON-Modus ohne Schema)"
  - `settings.tuning.structured_output.off`: "off (prompt only)" / "off (nur Prompt)"
  - `settings.provider.mineru_base_url_hint`: "URL of the MinerU API server (FastAPI), without /v1. The OCR prompt setting has no effect for this kind." / "URL des MinerU-API-Servers (FastAPI), ohne /v1. Das OCR-Prompt-Setting hat fuer diesen Kind keine Wirkung."
- [ ] **Step 5: recommendedModel** — in `frontend/src/modelCatalog.ts:257` add a mineru branch before the generic lookup: vision → `'mineru'`, text → `null` (match the function's existing return type/style).
- [ ] **Step 6: Verify** — `cd frontend && npm run build` → clean; then `npm run dev` smoke: Settings → Provider shows the mineru kind, text-model picker disappears when selected, tuning shows the two new fields for openai_compatible.
- [ ] **Step 7: Commit**

```bash
git add frontend/src
git commit -m "feat(frontend): mineru provider card + max_tokens/structured-output tuning UI

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

---

### Task 10: Docs + full verification

**Files:**
- Modify: `docs/FEATURE_REFERENCE.md` (provider section) and `docs/USER_GUIDE.md` (settings/provider chapter — locate via `grep -n "openai_compatible\|OpenAI-compatible" docs/FEATURE_REFERENCE.md docs/USER_GUIDE.md`)

**Interfaces:** none — documentation + gate.

- [ ] **Step 1: Document** — add to the provider documentation (English, matching the docs' language):
  - New provider kind `mineru`: MinerU API server, vision/OCR-only, base_url without `/v1`, health check = GET `/docs`, fixed model `mineru`, no token usage/cost statistics, OCR prompt setting has no effect.
  - New tuning fields for OpenAI/OpenAI-compatible: `max_output_tokens` (sent as `max_tokens`; reasoning tokens count toward it) and `structured_output` (`auto`/`json_object`/`off`, with automatic one-shot retry without `response_format` on schema-rejecting 400s).
  - Reasoning models: `<think>` blocks are stripped from OpenAI-compatible responses; a reasoning-only response fails with a parser-configuration hint. Known limitation: the document-chat API path does not apply `max_output_tokens` (worker stages do).
  - Example configuration for SGLang/MiniMax + MinerU (copy the JSON from spec §1).
- [ ] **Step 2: Full gate**

```bash
cargo fmt
cargo clippy --workspace 2>&1 | tail -5    # expect: no warnings introduced by this work
cargo test --workspace 2>&1 | tail -4      # expect: all green
cd frontend && npm run build && cd ..      # expect: clean build
git status --short                          # expect: ONLY intended files (never crates/archivist-ocr/src/lib.rs)
```

- [ ] **Step 3: Commit**

```bash
git add docs/FEATURE_REFERENCE.md docs/USER_GUIDE.md
git commit -m "docs: mineru provider kind + reasoning/structured-output tuning

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
git push origin main
```

- [ ] **Step 4: Report** — summarize to the operator what must happen outside this repo before end-to-end verification: deploy MinerU API server + SGLang MiniMax, create the two providers in Settings (spec §1 values), set `stage_models: ocr → mineru`, run one document through OCR+metadata, and execute the MineruClient contract-verification curl from Task 6 against the live server. Remind: Epic #322 stripper fixes (#323/#324/#326/#327) should land before production MinerU use (HTML tables in MinerU Markdown).

---

## Deviations from spec (documented)

- Spec §9 lists `mineru_request_is_multipart_with_page_png` and `mineru_sends_bearer_when_secret_configured` as tests; multipart internals and default headers are not observable without an HTTP mock server (repo has none for archivist-ai). Coverage instead: `extract_mineru_markdown` fixtures (Task 6), constructor reuse of the `OpenAiCompatibleClient` header pattern, and the live contract-verification curl (Task 6 note + Task 10 report).
- Spec §9 worker test `text_stage_with_mineru_kind_errors`: covered by the `chat_with_provider` Mineru arm (Task 7) whose error string is pinned in the Interfaces block; an async DB-pool-free unit test of `chat_with_provider` is not possible in the current test setup, so the arm is verified by compilation + the api-side twin arm sharing the same wording.
