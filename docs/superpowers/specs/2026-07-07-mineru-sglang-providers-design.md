# Spec: MinerU-Provider-Kind + Reasoning-Haertung fuer OpenAI-kompatible Backends (SGLang/MiniMax)

Datum: 2026-07-07 · Status: vom Nutzer freigegebenes Design · Scope: nur App-Seite (kein Deployment)

## 1. Ziel

Zwei lokale Backends sollen erstklassig nutzbar werden:

1. **SGLang mit `nvidia/MiniMax-M2.7-NVFP4`** als Provider fuer **alle Text-Verbraucher** (Metadaten-Stage, Doc-Typ-Vorklassifikator, Konsens-Check, Dokument-Chat, Prompt-Tester — alle haengen an `default_provider` bzw. `stage_models`).
2. **MinerU-API-Server** (FastAPI vor vLLM) als **OCR-Provider**: Seitenbild hin, fertiges Markdown zurueck.

Ziel-Konfiguration nach Umsetzung (Settings → Provider):

```jsonc
// settings.ai.providers (Auszug)
{ "name": "sglang-minimax", "kind": "openai_compatible",
  "base_url": "http://<omega>:<port>/v1",
  "default_text_model": "nvidia/MiniMax-M2.7-NVFP4",
  "tuning": { "max_output_tokens": 8192 } },
{ "name": "mineru", "kind": "mineru",
  "base_url": "http://<mineru-host>:<port>",
  "default_vision_model": "mineru" }
// settings.ai.default_provider = "sglang-minimax"
// settings.ai.stage_models = [{ "stage": "ocr", "provider": "mineru", "model": "mineru" }]
```

## 2. Architektur-Entscheid (festgelegt)

**Kein neuer Kind fuer SGLang** — es ist ein OpenAI-kompatibler Server; der bestehende `openai_compatible`-Pfad wird gehaertet (Reasoning-Strip, `max_output_tokens`, Structured-Output-Schalter). **Neuer Kind nur fuer MinerU**, weil das Protokoll (Multipart-Datei → Markdown) kein Chat-Completions-Dialekt ist. Ein Kind pro *Protokoll*, nicht pro *Produkt*. Abgelehnte Alternativen: eigener SGLang-Kind (Client-Duplikation, YAGNI); MinerU in `openai_compatible` pressen (URL-Magie, luegender Health-Check/Model-Sync).

## 3. Ist-Zustand (Anker, Stand 2026-07-07)

- Kind-Enum `AiProviderKind { Ollama, Openai, Anthropic, OpenaiCompatible }`: `crates/archivist-core/src/lib.rs:1979`; gespiegelt in `openapi/openapi.yaml:29`, generiert `frontend/src/api/schema.ts` (via `cd frontend && npm run generate:client`), Hand-Mirror `frontend/src/api/client.ts:56`, UI-Select `frontend/src/settings/sections/ProviderCard.tsx:65`, `frontend/src/settings/sections/tuning.tsx:23`, `ModelCatalogEditor.tsx:6`.
- Provider liegen als `AiProviderSettings`-Array im `settings`-JSONB (`key='runtime'`, Pfad `ai.providers`); Defaults `AiProviderSettings::default_providers()` `core:1823`.
- Stage-Auswahl: `provider_for_stage` `crates/archivist-worker/src/main.rs:4121` (Override via `settings.ai.stage_models`, sonst `default_provider`); Vision-Client-Bau `build_vision_client` `main.rs:4370`; Text `chat_for_stage` `main.rs:4244`.
- OpenAI-kompatibler Client: `OpenAiCompatibleClient` `crates/archivist-ai/src/lib.rs` — Chat-Payload `build_openai_chat_payload:563`, Vision `:874`, Antwort-Extraktion `choices[0].message.content` (`:854`, `:922`), Model-Sync `list_models:810`.
- Metadaten-JSON: Schema `schema_for_metadata` `ai:2081`; OpenAI-kompatibel sendet `response_format:{type:"json_schema", json_schema:{name,strict:true,schema}}` (`ai:600-621`); defensives Parsen `normalize_model_json` `core:3978`.
- Es gibt **kein** `<think>`/`reasoning_content`-Handling und es wird **nie** `max_tokens` gesendet.
- Provider-Test sendet echten Chat "Return OK." (`archivist-api/src/main.rs:2680`); Model-Sync-Dispatcher `discover_provider_models` `api:2267`; SSRF-Prüfung erlaubt private IPs (`core/src/ssrf.rs:51`).
- **Veraltete Kommentare** (behaupten, `response_schema` werde von OpenAI-kompatibel/Anthropic ignoriert): `ai:211-216`, `worker main.rs:2217-2219` — im Zuge dieser Arbeit korrigieren.

## 4. Feature 1: Provider-Kind `mineru`

### 4.1 Typen & Spiegelstellen

- `AiProviderKind::Mineru` (Wire-Wert `mineru`) in `core:1979`; alle `match provider.kind`-Arme in worker/api/ai ergaenzen (Compiler treibt die Liste).
- `openapi.yaml` Enum + `npm run generate:client` fuer `schema.ts`; Hand-Mirror `client.ts`; UI-Selects (`ProviderCard.tsx`, `tuning.tsx` Presets, `ModelCatalogEditor.tsx`).
- Neues Default-Preset (disabled) in `default_providers()`: `name:"mineru"`, `base_url:"http://localhost:8001"`, `default_vision_model:"mineru"`. `provider_base_url`-Default-Arm (`main.rs:4173`): `http://localhost:8001`.

### 4.2 `MineruClient` (archivist-ai), implementiert `VisionProvider`

- Request: `POST {base}/file_parse`, `multipart/form-data` mit Dateifeld (Seiten-PNG aus `ImageInput.bytes`, statischer Dateiname `page.png` — der Client sieht nur eine Seite pro Aufruf) plus Formfeldern fuer Markdown-Rueckgabe.
  **Vertrags-Verifikation:** exakte Feldnamen (Datei-Feld, `return_md`-artige Flags) und Response-Shape (`md_content` o. ae.) werden bei der Implementierung gegen die **deployte, gepinnte MinerU-Version** verifiziert und als JSON-Fixture-Test eingefroren (`extract_md_content`-Helper mit dokumentierten Kandidatenpfaden; unbekannte Shape → Fehler mit Body-Snippet).
- `VisionRequest.prompt`/`temperature`/`num_ctx` werden von MinerU ignoriert — dokumentierter Hinweis: das OCR-Prompt-Setting hat fuer diesen Kind keine Wirkung.
- Auth: optionaler Bearer aus dem bestehenden Secret-System (`resolve_secret`); Timeout aus `StageProvider.request_timeout_seconds`.
- Antwort: `AiResponse { text: <markdown>, raw_response: <geparstes JSON>, ... }`. Kein Token-Usage → Statistik bleibt fuer diesen Provider leer (bewusst; `sum_vision_usage` liefert None).
- Redirects: `redirect::Policy::none()` wie bei allen Clients.

### 4.3 Vision-only-Absicherung

- Text-Pfade (`chat_for_stage` im Worker, `chat_with_default_provider` in der API) → bei `kind == Mineru` klarer Fehler: `provider kind "mineru" is vision-only (OCR); select it only for the OCR stage`.
- UI blendet Text-Model-Picker aus (4.5).

### 4.4 Health-Check & Model-Sync

- `test_ai_provider` (`api:2680`): Arm fuer `Mineru` → `GET {base}/docs`, 2xx genuegt (kein Chat). Fehler inkl. Status.
- `discover_provider_models` (`api:2267`): `Mineru` → statisch `["mineru"]`; UI blendet den Sync-Button aus.

### 4.5 Frontend

- `ProviderCard.tsx`: Option „MinerU (OCR-Server)"; bei `kind === 'mineru'`: Text-Model-Feld ausgeblendet, Vision-Model fix `mineru` (readonly), Sync-Button ausgeblendet, Base-URL-Hilfetext („URL des MinerU-API-Servers, ohne /v1").

## 5. Feature 2: Reasoning-Handling im OpenAI-kompatiblen Client

Zentrale Extraktion fuer Chat **und** Vision (ersetzt die zwei direkten `choices[0].message.content`-Zugriffe `ai:854`, `:922`):

1. `raw = message.content` (String wie bisher).
2. `stripped = strip_think_blocks(raw)`: entfernt **alle** `<think>…</think>`-Paare (nicht-gierig, auch mehrere, auch mitten im Text); ein `<think>` **ohne** schliessendes Tag verwirft alles ab `<think>`. Tags lowercase (`<think>`/`</think>`), wie von MiniMax-M2/DeepSeek-R1 emittiert. Danach nur fuehrenden/anhaengenden Whitespace trimmen.
3. Ergebnis nicht leer → zurueckgeben.
4. Ergebnis leer:
   - Wenn `raw` ein `<think>` enthielt **oder** `message.reasoning_content` nicht leer ist → **Fehler**: `model returned only reasoning content and no final answer — check the server's reasoning-parser configuration`.
   - Sonst → leerer String wie bisher (legitim, z. B. leere OCR-Seite; `validate_ocr_text` prueft auf Dokumentebene).
5. Ein befuelltes `reasoning_content` **neben** nicht-leerem Content wird ignoriert (bleibt aber in `raw_response`/`ai_artifacts`, bestehende Redaction-Regeln unveraendert).

Damit funktioniert MiniMax-M2 auf SGLang **mit und ohne** `--reasoning-parser`.

## 6. Feature 3: `max_output_tokens` (Tuning)

- Neues Feld `ProviderTuning.max_output_tokens: Option<u32>` (`core:1758`-Struktur; serde default None; `resolve_tuning`-Fallthrough wie bestehende Felder).
- Anwendung: `build_openai_chat_payload` und `OpenAiCompatibleClient::vision` setzen `"max_tokens": n`, wenn konfiguriert — fuer die Kinds `Openai` und `OpenaiCompatible`. Ollama (nutzt `num_ctx`) und Anthropic (fix 4096) unveraendert.
- Default leer = heutiges Verhalten (Server-Default).
- UI (`tuning.tsx`): Number-Feld, sichtbar fuer `openai`/`openai_compatible`; Hilfetext: „leer = Server-Default. Achtung: Reasoning-Tokens zaehlen bei den meisten Servern mit — fuer Denk-Modelle grosszuegig waehlen (z. B. 8192)."

## 7. Feature 4: Structured-Output-Schalter + Selbstheilung

- Neues Tuning-Feld `structured_output: StructuredOutputMode { Auto, JsonObject, Off }` (Wire: `auto`/`json_object`/`off`, Default `Auto`).
- Wirkung in `build_openai_chat_payload`, wenn `response_schema` gesetzt ist:
  - `Auto`: heutiges Verhalten — `response_format: json_schema (strict:true)`.
  - `JsonObject`: `response_format: {"type":"json_object"}` (Schema-Anforderungen stehen weiterhin im Prompt; `normalize_model_json` parst defensiv).
  - `Off`: kein `response_format`.
- **Selbstheilung** (nur `Auto`): HTTP 400, dessen Body case-insensitiv auf `response_format`/`json_schema`/`grammar`/`schema` hindeutet, und die Anfrage enthielt `response_format` → **einmaliger** Retry derselben Anfrage ohne `response_format` + `warn!`-Log. Entscheidung als pure Funktion `should_retry_without_schema(status, body, had_response_format) -> bool` (testbar), Wiring im Client.
- Kommentar-Fixes: `ai:211-216` und `worker main.rs:2217-2219` (siehe §3 letzter Punkt).
- UI (`tuning.tsx`): Select, sichtbar fuer `openai`/`openai_compatible`, mit Kurzbeschreibung der drei Modi.

## 8. Worker-Integration

- `build_vision_client` (`main.rs:4370`): Arm `Mineru` → `VisionClient::Mineru(MineruClient)`; `VisionClient`-Enum erweitern.
- OCR-Loop **unveraendert**: Seitenbilder, Page-Cache (Key = sha256 der gerenderten Bytes), Lease-Heartbeat. Bewusst **kein** PDF-Direktupload an MinerU (Cache-/Retry-Semantik bleibt intakt; Non-Goal).
- Vision-Fallback bleibt Ollama-only (`is_vision_model_runtime_crash`, `installed_ollama_models_for_provider` liefern fuer Mineru nichts — vorhandenes Verhalten fuer Nicht-Ollama). MinerU-Fehler laufen in bestehende Retry/Cooldown-Mechanik; Fehlermeldung enthaelt HTTP-Status + Body-Snippet.
- `ollama_*_num_ctx_for_provider` liefern fuer Mineru `None` (bestehende Nicht-Ollama-Logik).

## 9. Tests

archivist-ai (Unit):
- `strip_think_removes_single_block`, `..._multiple_blocks`, `..._leading_block`, `..._unclosed_block_drops_tail`
- `reasoning_only_content_errors`, `reasoning_content_alongside_text_is_ignored`, `empty_content_without_reasoning_stays_ok`
- `openai_chat_payload_sets_max_tokens_when_tuned`, `..._omits_max_tokens_by_default`, `openai_vision_payload_sets_max_tokens_when_tuned`
- `structured_output_json_object_mode`, `structured_output_off_mode`, `should_retry_without_schema_matrix`
- `mineru_request_is_multipart_with_page_png`, `mineru_extracts_markdown_from_fixture`, `mineru_error_includes_status_and_body_snippet`, `mineru_sends_bearer_when_secret_configured`

archivist-worker (Unit):
- `provider_for_stage_uses_mineru_override_for_ocr`, `text_stage_with_mineru_kind_errors`, `provider_base_url_default_for_mineru`

archivist-api (Unit):
- `discover_provider_models_returns_static_for_mineru`, Provider-Test-Arm fuer Mineru (Funktionsebene)

Frontend: `tsc`/Build gruen; ProviderCard rendert `mineru`-Kind ohne Text-Model-Picker.

End-to-End (nach Konfiguration der echten Endpoints, ausserhalb CI): 1 Dokument durch MinerU-OCR + MiniMax-Metadaten; Chat-Frage gegen ein Dokument (Think-Strip sichtbar wirksam).

## 10. Nicht-Ziele

- Kein MinerU-/SGLang-Deployment (separater Schritt; App ist gegen beliebige URLs konfigurierbar).
- Kein Cross-Provider-Fallback (MinerU ↔ Ollama); Ausfaelle → bestehende Retry/Cooldown-Mechanik.
- Kein PDF-Direktupload an MinerU; keine SGLang-Spezialflags (`chat_template_kwargs` etc.); keine Embeddings; kein Streaming.

## 11. Abhaengigkeiten & Risiken

1. **Markup-Stripper (Epic #322):** MinerU liefert Tabellen teils als HTML-`<table>` im Markdown — genau der Pfad von `strip_ocr_layout_markup`. Vor produktiver MinerU-Nutzung mindestens #323, #324, #326, #327 fixen, sonst verstuemmelt der Stripper MinerU-Tabellen. (Die Basis-Aenderung in `crates/archivist-ocr` ist Stand 2026-07-07 uncommitted.)
2. **MinerU-API-Vertrag** versionsabhaengig → bei Implementierung gegen gepinnte Version verifizieren, Fixture-Test (siehe 4.2).
3. **SGLang + MiniMax + `json_schema`**: Grammar-Backend-Verhalten mit Thinking live testen; Absicherung durch `structured_output`-Schalter + 400-Selbstheilung (§7).
4. `max_tokens` inkl. Reasoning-Budget: zu kleiner Wert kann Antworten koepfen → UI-Hinweis (§6), Default bleibt Server-seitig.

## 12. Umsetzungshinweise

- Reihenfolge: (1) Reasoning/`max_tokens`/`structured_output` im OpenAI-Pfad inkl. Tests → macht `sglang-minimax` sofort nutzbar; (2) `mineru`-Kind end-to-end; (3) Frontend; (4) Doku (`docs/FEATURE_REFERENCE.md`/`USER_GUIDE.md`-Abschnitt Provider).
- Repo-Konventionen: Commit + Push pro Phase; `frontend/package.json`-Version beim Release-Tag bumpen; Release-Notes-Eintrag.
