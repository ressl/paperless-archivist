<!-- Full prod audit 2026-06-10 (Fable-5 agent team: 7 dimensions investigated, adversarially verified, synthesized over the live deployment + v1.12.3 code). Tracked as GitLab milestones "Prod-Audit 2026-06: P0-P3" + issues #299+. Personal/topology identifiers redacted for the public mirror. -->

# Audit-Abschlussbericht — paperless-archivist v1.12.3 / PostgreSQL 18.3
Stand der Verifikation: 2026-06-10 ~20:50 UTC, live auf Prod (the CNPG primary, read-only) + Code-Stand v1.12.3. Alle Befunde unabhängig verifiziert; widerlegte Teilbehauptungen sind entfernt bzw. als Korrektur vermerkt.

## 0) Bottom line

**Läuft es sauber? Betrieblich ja — aber nicht fehlerfrei: ein kritischer OIDC-Bug ist JETZT scharf (der nächste Login entzieht dem einzigen Admin erneut die Rechte), die Statistik untertreibt Tokens um ~40 %, und ~10 % der Inventar-Statusanzeigen sind falsch.** Plattform, DB-Gesundheit und Kern-Aggregation sind dagegen nachweislich solide; der Quota-Incident ist behoben und der Backlog drainiert.

**1 kritisch · 6 hoch · 13 mittel · 16 niedrig · 13 info** (Info überwiegend Positiv-/No-Action-Befunde).

Sofort (vor allem anderen):
1. OIDC-Workaround **vor dem nächsten Login**: ZITADEL „User Info inside ID Token" aktivieren oder Subject `<oidc-subject-id>` in die Admin-Allowlist + sub-Matching (§1.1).
2. Die 20 permanent gefailten Dokumente via `/operations/unblock` re-queuen (§1.5).
3. `ARCHIVIST_METRICS_TOKEN` im Deploy-Repo setzen + Alert auf `ai.provider_quota_exhausted`-Rate (§5.2).
4. Nächstes Release: num_ctx-Floor am Point-of-Use (§1.2) + OCR-Token-Backfill (§2.2).

## 1) Bugs & Korrektheit

**[KRITISCH] OIDC-Admin-Deprovisionierung: Subject-Fallback-Username matcht die Admin-Allowlist nie, und das autoritative Rollen-Replace (#289) strippt Admin bei jedem Login.**
Username-Fallback `preferred_username → verified email → sub` (main.rs:927-934); `oidc_is_admin` matcht nur username/verified-email, nie `sub` (main.rs:6736-6751, NOTE #291); kein userinfo-Fetch. Liefert ZITADEL die Claims nicht im ID-Token, wird der User unter dem rohen Subject geführt und beim Returning-Login werden die Rollen per DELETE+INSERT ersetzt (archivist-db lib.rs:712-726) → Admin weg. `ensure_bootstrap_admin` greift nicht mehr, sobald ein User existiert (main.rs:525-527) → in-band nicht behebbar. *Prod-Beweis:* `user.oidc_created` 2026-05-14 mit Username `<oidc-subject-id>` und roles=[viewer] (Subject-Fallback feuerte real); xmin-Forensik: Login 2026-06-10 17:22Z schrieb user_roles auf [viewer] (xmin 13130248/13130250 gleicher Tx-Burst), Admin-Zeile danach manuell per SQL re-inserted (xmin 13132420; 0 `user.roles_changed`-Events). OIDC-Logins laufen ~täglich weiter — der manuelle Admin-Grant wird beim nächsten Login wieder gestrippt.
*Fix:* Sofort ZITADEL-Option „User Info inside ID Token" oder Subject in `ARCHIVIST_OIDC_ADMIN_USERS` + sub-Matching. Code: (a) userinfo-Fetch bei degradierten Claims, (b) Allowlist gegen immutables `sub`, (c) bei degradierten Claims (kein preferred_username, keine verified email) bestehende Rollen behalten statt neu berechnen, (d) Last-Admin-Demotion verweigern + Break-Glass-Local-Admin.

**[HOCH] Text-num_ctx-Floor (16384) per Provider-Override umgehbar — das shipped Ollama-Preset pinnt 4096; exakt der v1.12.2-Incident kehrt zurück.**
`resolve_tuning` bevorzugt Provider- vor Global-Wert (core lib.rs:891-893); `ollama_default()` pinnt `text_num_ctx=Some(4096)` (core lib.rs:1838; eigener Unit-Test core lib.rs:4923 beweist, dass der Override gewinnt). Text-Pfad ohne Floor (worker main.rs:4116-4121, genutzt in chat_for_stage:4106 und run_consensus_check:2860), Vision floort mit `.max(16384)` (main.rs:4132-4137) — echte Asymmetrie. `bump_text_num_ctx_if_too_small` fixt nur den Global-Wert (db lib.rs:8005). Prod ist nur zufällig safe (Provider-Override NULL → global 16384); eine Fresh-Install auf dem Ollama-Preset ist „born broken".
*Fix:* `.max(16384)` am Point-of-Use (beide Call-Sites) bzw. Clamp in `resolve_tuning`; Preset-Wert auf ≥16384 anheben.

**[MITTEL] Rollen-Replace bei wiederkehrenden OIDC-Usern erzeugt kein Audit-Event — die Prod-Demotion hinterließ null Spur.**
`upsert_oidc_user` ruft `replace_user_roles_tx` unconditional (lib.rs:726), Audit nur bei `created || linked_existing` (lib.rs:728-757). Prod: genau 2 `user.*`-Events (beide 2026-05-14), obwohl der Login vom 10.06. die Rollen nachweislich umschrieb; `auth.oidc_login_success` loggt nur den DB-Username und maskiert die degradierten Claims (main.rs:973-977).
*Fix:* Rollen-Diff bei jedem Login; bei Änderung `user.roles_replaced`-Event (before/after) + WARN, welche Claims im ID-Token fehlten.

**[MITTEL] Abgelaufene Leases bekommen keine Claim-Priorität — gecrashte In-Flight-Jobs warten hinter dem gesamten Backlog.**
`claim_jobs` reklamiert expired Leases (db lib.rs:5018-5019, `job.lease_reclaimed` heute nachgewiesen), aber das ORDER BY (lib.rs:5027-5031) boostet nur `error_message IS NOT NULL AND attempts>0` — abandoned Jobs haben error_message=NULL. Live: Job 019ea2ea-3760 (Doc 5403) 3,8 h+ verhungert, Rang 85 der Claim-Reihenfolge bei ~15,8 Jobs/h; Recovery-Zeit skaliert mit Backlog-Länge. (Wurde später regulär reclaimed und abgeschlossen — Fairness-/Latenzproblem, kein Datenverlust.)
*Fix:* ORDER-BY-Tier voranstellen: `CASE WHEN status='running' AND lease_until<now() THEN 0 ...`, oder Reaper, der expired-running auf 'queued' zurücksetzt (created_at erhalten).

**[MITTEL] 20 Dokumente permanent gefailt durch Attempt-Erschöpfung bei gestapelten transienten Paperless-Gateway-Ausfällen (10.06.).**
Alle 20 mit `status=404, body=404 page not found` im Fenster 12:54:30-12:55:45Z, attempts=3=max; Klassifikation als transient korrekt (archivist-paperless lib.rs:54-67, #245), aber transiente Infrastruktur-Fehler verbrauchen Attempts: 503 11:59Z → Gateway-404 12:52Z → failed 12:54Z. Wichtig: Quota-429s verbrauchten korrekt KEINE Attempts (v1.12.2-Exemption wirkt) — alle 3 Attempts gingen allein am 10.06. drauf.
*Fix:* Die 20 Docs via `/operations/unblock` re-queuen; Gateway-unreachable/5xx-transient keine Attempts berechnen (analog Quota-Exemption).

**[MITTEL] unblock-jobs löscht Provider-Cooldowns, released aber die davon geparkten Jobs nicht — der „(prod-blocked)"-Half-Fix ist wieder da.**
`unblock_jobs_endpoint` cleart Cooldowns by default (main.rs:5379-5405), ruft aber nie `release_scheduled_retries`; der Worker parkt Jobs auf `run_after=cooldown_until` (worker main.rs:621, 1087, 1394) → sie schlafen bis zum alten Expiry weiter. Verschärfend: der Fallback-Button liegt im MaintenanceDrawer (nur canManageSettings, Dashboard.tsx:311-323) — ein Operator hat gar keinen UI-Pfad.
*Fix:* Nach Cooldown-Clear auch `release_scheduled_retries` aufrufen und `released` in Response/Audit aufnehmen (Spiegel von clear_provider_cooldowns_endpoint:5491).

**[MITTEL, Impact korrigiert] reject_oversized_pages prüft nur Seite 1 — übergroße Folgeseiten umgehen den Render-Pixel-Cap (#297).**
`pdfinfo <input>` ohne `-f/-l` (ocr lib.rs:171) druckt nur die Seite-1-Größe (empirisch mit 2-Seiten-PDF bestätigt); pdftoppm rendert aber bis page_limit=3. Korrektur zum Rohbefund: kein Multi-GB-/tmp-Fill — poppler kappt bei ~26666 px („Bogus memory allocation size"), Blank-Riesenseite komprimiert auf ~2 MB; reale Folge auf dem 1Gi-Worker ist ein OOM-gekillter pdftoppm-Child als retrybarer Poison-Pill (~2,2 GB/Seite gebunden).
*Fix:* page_limit durchreichen und `pdfinfo -f 1 -l <page_limit>` aufrufen (der Parser kann die per-Page-Zeilen bereits); optional pdftoppm-Memory-Cap.

**[MITTEL, latent] Konfigurierbarer AI-Timeout (prod 600 s) > hartkodierte 300-s-Lease — Lease läuft mid-call ab, Reclaim/Doppelverarbeitung wird möglich.**
Lease hart 300 s (worker main.rs:550 + alle bump_job_lease-Sites); `request_timeout_seconds` ungeclampt angewendet (main.rs:4145; core lib.rs:949-952 ohne Ceiling); Heartbeats nur ZWISCHEN Calls. Auf aktueller Topologie nicht ausnutzbar: 1 Worker-Replica ohne HPA, Prozess-interner Reentry-Guard, Stage-Jobs laufen 76-145 s. Aktiviert sich bei Scale-out auf 2 Replicas oder massiver Modell-Degradation.
*Fix:* `lease_seconds = max(300, request_timeout + Marge)` bei claim und bump, und/oder Heartbeat-Task während langer Calls; mindestens Timeout-Ceiling clampen und Kopplung dokumentieren.

**[NIEDRIG] Modellwechsel-Auto-Release weckt ALLE future-dated queued Jobs — kollabiert Backoff+Jitter unbeteiligter Transient-Retries.**
`release_scheduled_retries` ohne Cooldown-Qualifier (db lib.rs:8599-8600), feuert bei jedem Provider/Modell-Change (api main.rs:1561-1573); trifft nachweislich beide Park-Mechanismen (Cooldown worker main.rs:1102-1104, Backoff+Jitter db lib.rs:5395-5408). Bei concurrency=1 derzeit folgenlos (Blast-Radius aktuell 0). *Fix:* Release auf cooldown-geparkte Jobs scopen (Tagging oder run_after > max. reguläres Backoff-Fenster).

**[NIEDRIG] RBAC-Inkonsistenz: Operator darf `/provider-cooldowns/clear` nicht (WriteSettings), wiped Cooldowns aber via `/unblock-jobs` (WriteRuns).**
main.rs:5481 vs. :5393+:5401-5405; Rollenmatrix core lib.rs:533-541. *Fix:* Eine Permission für Cooldown-Manipulation auf beiden Endpoints (WriteRuns passt semantisch).

**[NIEDRIG] update_settings persistiert und returned UN-normalisierte Settings — PUT-Response ≠ nächstes GET.**
Persist verbatim (main.rs:1545, Audit mit Rohwert), Normalisierung nur beim Read (db lib.rs:1392-1394; Clamp-Beispiel core lib.rs:984-989); Kontrast: update_workflow_controls normalisiert vor Persist (main.rs:3343); Frontend übernimmt die PUT-Response als State (SettingsPage.tsx:358-361). *Fix:* `request.settings.normalized()` vor `update_runtime_settings`, normalisiert returnen.

**[NIEDRIG] TuningNumberField akzeptiert Negative/Brüche für Option<u32>-Felder — Save scheitert mit opakem Body-422.**
Commit per Keystroke ohne Integer/Min-Enforcement (tuning.tsx:429-437; helpers.ts:24-28); kein JsonRejection-Handler in main.rs → axum-Default-422 ohne Feldbezug blockiert den gesamten Save. *Fix:* Clamp/Round on commit (Blur-Commit-Pattern wie NumberField) oder Json-Rejections auf Feld-400s mappen.

**[NIEDRIG] Prompts-i18n-Reste: hartkodiertes „ (active)" und unlokalisierbare Status „draft"/„valid".**
promptOptionLabel hartkodiert (Prompts.tsx:457-459, genutzt :175/:382); status.draft/status.valid/status.active fehlen in messages.ts:632-648 → englischer Fallback in allen 7 Locales. *Fix:* prompts.active_marker verwenden; Status-Keys in allen Locales ergänzen.

**[NIEDRIG] Ungespeicherte Prompt-Edits werden bei „Activate selected" kommentarlos verworfen.**
Editor-Reset-Effekt feuert auf neue Array-Identitäten nach load() (Prompts.tsx:92-108 via :32-48); Activate-Button ohne promptDirty-Guard (:233-239). *Fix:* Reset bei promptDirty skippen / Effekt auf stabile IDs keyen / Confirm vor Verwerfen.

**[INFO] release_scheduled_retries_endpoint ohne `#[tracing::instrument]` — `Span::record("user_id")` ist ein No-op** (main.rs:5522/:5531 vs. Siblings :5313-5316). *Fix:* Attribut wie bei den Siblings ergänzen.

## 2) Statistik-Korrektheit

**[HOCH] Der gewählte Endtag fehlt immer: bare-date `to` → 00:00:00Z + exklusives `< to`, und die UI sendet immer `to=heute` — der laufende Tag ist auf der gesamten Seite unsichtbar; Single-Day-Range liefert 400.**
parse_stat_datetime mappt YYYY-MM-DD auf UTC-Mitternacht (api main.rs:3084-3091); beide Queries half-open (db lib.rs:3188-3190, 3244-3246); Frontend sendet immer `to=isoDay(new Date())` (Statistics.tsx:76, 110); `from>=to` → 400 (main.rs:3157-3159). Live 20:50Z: 71 Requests / 80.544 Output-Tokens von heute in jedem Preset/Default unsichtbar (Dashboard zeigt sie). Nuance: zwischen 00:00-02:00 lokal (CEST) liegt die Grenze in der Zukunft, dann inkludiert; danach nicht mehr.
*Fix:* Bare-date `to` serverseitig als exklusives Tagesende (Datum + 1 Tag) parsen — fixt zugleich den from==to-400; alternativ `to` bei Presets weglassen (Backend default = now). Regressionstest: heutige Daten in der Default-View.

**[HOCH] Alle 6.061 Bestands-OCR-Artefakte zählen 0 Tokens — Headline „Output tokens" ~40 % zu niedrig; der #259-Fix ist forward-only ohne Backfill.**
Queries lesen nur top-level usage.*/eval_count (db lib.rs:3174-3185, 3089-3103), kein pages[]-Fallback; Flatten nur beim Insert (worker main.rs:1686-1701); Migrationen 0033-0036 ohne Backfill. Live: Nested-Recount via lateral = 7.065.928 Output / 3.418.386 Input über 10.353 Page-Calls; wahres Fenster-Total ≈ 17,8 M statt angezeigter 10,7 M (−39,7 %); letztes OCR-Artefakt 06.06. → derzeit 100 % der OCR-Historie ungezählt („ocr | 6.061 | 0").
*Fix:* Einmaliger Backfill (`UPDATE ai_artifacts SET response = response || jsonb usage aus pages[] WHERE stage='ocr' AND response ? 'pages' AND NOT response ? 'usage'`) oder lateral-Fallback in beiden Queries. Strategisch besser: typed Token-Spalten (§3.4) — der Backfill dort erledigt das mit.

**[MITTEL] „24h"-Preset zeigt den gestrigen UTC-Kalendertag, nicht die letzten 24 Stunden.**
`presetFrom('24h') = isoDay(now−24h)` (Statistics.tsx:59-60) + `to=heute` → Fenster [gestern 00:00Z, heute 00:00Z). Live: Preset = 284 Requests, echte Rolling-24h = 71 — 4,0× Inflation; zum Messzeitpunkt fehlten 100 % der echten Last-24h-Aktivität. Hälfte des Fehlers ist die Endtag-Exklusion (s. o.); der Rest bleibt auch danach (~48-h-Fenster mit Label „24h").
*Fix:* Für 24h volles RFC3339-`from` (`new Date(now-24h).toISOString()`) senden und `to` weglassen; Rolling-RFC3339 auch für 7d/30d/90d erwägen, bare Dates nur für manuelle Picker.

**[NIEDRIG] Input-Tokens vor 2026-06-10 15:40Z dauerhaft 0 — die alte Redaction (#246-Ära) hashte prompt_eval_count unwiederbringlich weg.**
7.284 Metadata-Artefakte mit `{"redacted": true, "sha256": ...}`; post-Cutover dominiert Input ~5,5:1 (457.678 in / 83.126 out seit 15:40Z) → eine künftige Kostenkonfiguration würde historische Kosten mehrheitlich auslassen. Aktuell harmlos (alle cost-Felder null → „—"; client.ts:706-708 dokumentiert es).
*Fix:* Als Datenqualitätsgrenze akzeptieren; falls Pricing konfiguriert wird, estimated_cost für Ranges vor dem Cutover flaggen/unterdrücken („Kosten unvollständig vor <Datum>") statt falscher Zahl.

**[NIEDRIG, Attribution korrigiert] Jobs-Throughput ist retroaktiv mutabel: Buckets nach CURRENT status @ updated_at, nicht nach historischem Ereignis.**
db lib.rs:3236-3249; transiente Fails erscheinen nie als „failed" (zurück auf queued, lib.rs:5401-5418); live exakt bestätigt (538 succeeded mit attempts>1; 1.100 terminale Jobs mit updated_at > created_at+7d). failed→queued-Rewrites existieren — aber via Startup-Vision-Crash-Requeue (worker main.rs:462-470) und Operator-Unblock (api main.rs:5396, resettet attempts), NICHT via release_scheduled_retries (preserved status, lib.rs:8593-8607) — die v1.12.2-Attribution des Rohbefunds war falsch.
*Fix:* Semantik auf der Seite dokumentieren („aktueller Ausgang terminal gewordener Jobs im Zeitraum") oder Throughput aus immutabler Quelle ableiten (audit_events / pipeline_runs-Transitions).

**[NIEDRIG] Leere Buckets werden weggelassen (kein Zero-Fill) und die Charts nutzen eine Category-X-Achse — Lücken unsichtbar, Zeitachse verzerrt.**
Handler baut Series nur aus zurückgegebenen Buckets (api main.rs:3193-3226; Kontrast: dashboard_bucket_labels zero-fillt, db lib.rs:3270+); XAxis dataKey="label" + monotone-Interpolation (Statistics.tsx:231, 261, 280). Live: nur 13 von 30 Tages-Buckets vorhanden → über die Hälfte der Achse kollabiert. Per-Bucket-Werte selbst korrekt.
*Fix:* Buckets from..to in der gewählten Granularität enumerieren und zero-fillen (server- oder clientseitig).

**[NIEDRIG] Ungültige from/to fallen still auf Defaults zurück (200 statt 400); p95 wird berechnet, aber nie returned; /api/statistics fehlt komplett in openapi.yaml.**
`.and_then(parse_stat_datetime).unwrap_or(default)` (main.rs:3146-3156) — `?from=2026-13-99` liefert kommentarlos das 30-Tage-Default; percentile_cont(0.95) in SQL (db lib.rs:3187) ohne jeden Consumer (UsageAgg main.rs:3093-3135, kein Frontend-Bezug). Bundled UI unbetroffen (Picker erzeugen valide Werte).
*Fix:* 400 bei vorhandenen, aber unparsebaren Parametern (Defaults nur bei Absenz); p95 droppen oder surfacen; Endpoint in openapi.yaml dokumentieren.

**[INFO] Positiv: Innerhalb des Fensters ist die Aggregation ziffergenau korrekt.** Unabhängige Replika-Nachrechnung (Zellreplikat + Flat-Aggregat) = 13.350 Requests / 10.733.391 Output / 19.538,754 ms avg, Throughput 12.614/2.695/428 — identisch. Negativchecks: 0 Rows mit mehr als einem Token-Pattern (additive Summe derzeit safe), 0 NULL-duration_ms, TZ=UTC, kein Modell über zwei Provider, Kosten-KPI korrekt „—". Wachsamkeitspunkt: die additive Pattern-Summe würde doppelt zählen, falls je usage.* UND eval_count in einer Response auftauchen.

**[INFO] Document-Chat-Calls landen nie in ai_artifacts → Statistik exkludiert sie (aktuell 2 Messages, vernachlässigbar).** API ruft provider.chat() direkt (main.rs:2563, 2583); insert_ai_artifact nur im Worker. *Fix bei Bedarf:* stage='chat'-Artefakt schreiben oder Exklusion dokumentieren.

## 3) DB-Layout & Indizes

**[HOCH] document_inventory.current_run_status ist ein handgepflegter Mirror von pipeline_runs.status — aktuell für ~10 % der Zeilen falsch.**
Zwei Write-Sites mirrorn nicht: release_lease_for_cooldown (worker main.rs:1099-1118) und reset_stuck_running_pipeline_runs (db lib.rs:8093-8124); die Stale-Lease-Requeue-Site mirrort dagegen (lib.rs:6904-6927) — der Mirror ist per-Call-Site-Disziplin. Prod: ~600/5.996 Zeilen zeigen „running", tatsächlich queued (pipeline_runs: queued=602, running=1; 601/601 mit last_run_id=pr.id gegengeprüft). Konsumenten des falschen Werts: Inventory-Filter (lib.rs:4163-4168), Stage-Running-Count (:3535), KPI (:2406).
*Fix:* Spalte abschaffen — `pipeline_runs_one_active_per_document_idx` (0001_init.sql:181-183) garantiert genau einen aktiven Run pro Dokument → Lateral-Join/View in list_inventory + Dashboard-Counts. Falls die Spalte bleibt: ein einziger Writer (SQL-Funktion oder Trigger) + periodischer Reconcile. Hotfix sofort: beide Sites mirrorn + einmaliges Repair-UPDATE der ~600 Zeilen.

**[MITTEL] Sechs fossile Per-Field-Statusspalten („unknown" auf 100 % der 5.996 Zeilen), stale document_backlog-View und ihr toter Composite-Index überlebten die v1.4.0-Konsolidierung.**
Group-by über alle sechs Spalten = exakt 1 Bucket; die View referenziert per 0028 entfernte Stages und hat 0 Referenzen in crates/, frontend/, openapi/; document_inventory_status_idx idx_scan=3 / 200 kB. Toter Code konsumiert die Spalten noch: Failed-KPI-OR-Kette lib.rs:2405, missing_*-Zähler :2398-2403, stage_breakdown-UNION-Arme :3514-3524.
*Fix:* Eine Migration: View, sechs Spalten, Index droppen; upsert_inventory und die drei Code-Konsumenten bereinigen. Falls eine Backlog-View gewünscht: über ocr_status+metadata_status neu definieren.

**[MITTEL] ai_providers-Tabelle ist tot — Provider-Config lebt im settings-jsonb; zwei Quellen der Wahrheit, bereits divergiert (4 vs. 5 Einträge).**
0 Code-Referenzen (grep über crates/, frontend/, openapi/ — nur Migrationen); 0003 seedete beide Stores identisch; Rows seit Bootstrap eingefroren. Der Freitext `ai_provider_cooldowns.provider_name` ohne FK passt zur bekannten #279-Name-Mismatch-Bug-Klasse.
*Fix:* Entweder Tabelle droppen und jsonb als Single Source akzeptieren, oder zur autoritativen Quelle machen mit FK von ai_provider_cooldowns — nicht beides behalten. (Verwandte Hygiene: keylose Seeds, §6.3.)

**[MITTEL] Token-Usage wird bei jeder Query per 6-Branch-CASE aus ai_artifacts.response-jsonb geparst — 4× copy-paste; jeder 30d-Render de-toastet praktisch die ganze Tabelle (24 MB jsonb von 50 MB).**
Byte-identische CASE-Blöcke: lib.rs:2980-2993, 3089-3103, 3174-3185, 3826-3844 (Letzteres als korrelierte Subquery pro Bucket); kein cost-Feld (hardcoded None :3049). Die fünf Statistik-Aggregationen sind die einzigen häufigen Statements >50 ms (79,5-126 ms).
*Fix:* input_tokens/output_tokens bigint beim Insert befüllen (der Worker parst die Response ohnehin), Backfill mit dem existierenden CASE (erledigt §2.2 mit), die vier Aggregate auf typed Spalten umschreiben. Künftige USD-Kosten als numeric(12,6), nie float.

**[MITTEL] pipeline_runs & jobs haben als einzige Tabellen keine Retention — unbegrenztes Wachstum mit CASCADE-Falle; nebenbei hasht claim_jobs pro Poll ~3,5 k terminal-failed Jobs.**
Kein `delete from pipeline_runs/jobs` im gesamten Code (alle anderen Stores haben Retention, lib.rs:7337-7421); ~405 Runs/Tag, 10.942 Runs / 17.407 Jobs in 27 Tagen. FK-Regeln (pg_constraint): jobs/ai_artifacts/review_items CASCADE — Run-Pruning löscht Review-Historie mit; audit SET NULL. claim_jobs-Plan (EXPLAIN, read-only): Anti-Join über {queued,running,waiting_review,failed}, rows=4359, heute 0,94 ms — Kosten O(queued+failed); das „failed"-Inkludieren ist korrekte Stage-Ordnung, also ist Retention (nicht Index/Prädikat) der richtige Hebel.
*Fix:* apply_security_retention um `runs_retention_days` erweitern (terminale Runs, bounded batches; jobs via CASCADE); vorher review_items.run_id auf ON DELETE SET NULL flippen und den FK aus §3.8 anlegen. Bei >10k Backlog claim_jobs re-EXPLAINen.

**[MITTEL] Index-Totgewicht ~25 MB auf den heißesten Insert-Tabellen — aber audit_events_job_id_idx BEHALTEN (Drop-Empfehlung widerlegt).**
Per idx_scan=0 UND Code-Shape-Beweis über alle audit_events-Query-Sites droppbar: audit_events_hash_idx 10 MB (seit 0034 ist die Kette über chain_position verankert — lib.rs:7128-7135, 7245-7256; EXPLAIN bestätigt chain_position_idx), audit_events_actor_created_idx 5,9 MB, audit_events_document_created_idx 4,8 MB (kein App-Query filtert actor/document). Dazu review_items_suggested_patch_gin_idx 344 kB (keine Containment-Operatoren existieren) und document_inventory_paperless_modified_idx 304 kB (Delta-Sync filtert API-seitig via modified__gt, worker main.rs:3863-3866 — bleibt auch mit aktiviertem Delta-Sync tot). ai_artifacts_normalized_output_gin_idx 3,4 MB ist jsonb_path_ops und kann die einzigen real existierenden Query-Shapes (->>/#>>-Extraktion + `? 'prompt_experiment_group'` lib.rs:1612) konstruktionsbedingt nicht bedienen. **Widerlegt:** audit_events_job_id_idx ist planner-reachable — EXPLAIN zeigt BitmapAnd(type_created + job_id) für den Provider-Feedback-Join bei ≥30-Tage-Ranges → behalten; run_id_idx behalten (lib.rs:8456).
*Fix:* Migration mit den 5-6 Drops (normalized_output ggf. als jsonb_ops neu, falls list_prompt_experiments je Beschleunigung braucht); die tagesjungen 0036-trgm-Indizes nach ~30 Tagen neu bewerten (Methodik 0035/#273); pg_stat_user_indexes ein Release später gegenchecken.

**[NIEDRIG, mit Korrekturen] Keine CHECK-Constraints auf den Status-Textspalten von document_inventory, während pipeline_runs/jobs ge-CHECKt sind.**
0 contype='c' auf document_inventory/paperless_sync_state/audit_events (vs. 0001_init.sql:166-192); Live-Werte derzeit sauber. Zwei Korrekturen zum Rohbefund: audit_events.outcome hat VIER Live-Werte (success 55.567 / retry 15.585 / failed 10.838 / warning 19) — der vorgeschlagene 2-Werte-CHECK würde beim VALIDATE scheitern bzw. den Retry-Writer brechen; und ein Domain-CHECK hätte die §3.1-Drift NICHT gefangen („running" liegt im gültigen Wertebereich). Hardening-Lücke, kein Defekt.
*Fix:* Nach dem Spalten-Drop (§3.2) CHECKs als NOT VALID + VALIDATE nachziehen; Wertemengen aus Live-DISTINCTs ableiten, nicht aus Annahmen.

**[NIEDRIG] document_inventory.last_run_id ohne FK auf pipeline_runs, obwohl zwei Write-Pfade darauf korrelieren.**
0001_init.sql:155 ohne REFERENCES; complete_job-Reset (lib.rs:5326-5337) und Metadata-Backfill-Nudge (lib.rs:7809-7823) no-open bei Dangling still — gleiche Fehlerklasse wie die Mirror-Drift. Live-Orphans: 0 (auch correspondent_id/document_type_id) → FK heute gratis anlegbar.
*Fix:* FK ON DELETE SET NULL als NOT VALID + VALIDATE; zwingend VOR der runs-Retention (§3.5), sonst stranden Pointer.

**[NIEDRIG] document_inventory.document_date ist text mit ISO-Dates.**
0012_standard_metadata.sql:2; 5.996/5.996 non-null, 0 Verstöße gegen `^[0-9]{4}-[0-9]{2}-[0-9]{2}$` → `USING document_date::date` verifiziert gefahrlos. *Fix:* auf date altern + Rust Option<NaiveDate>; minimal Format-CHECK. Billig, solange die Daten sauber sind.

**[INFO] Positiv: keine fehlenden Indizes auf den Hot-Paths** (einzige häufige Statements >50 ms sind die jsonb-Aggregationen aus §3.4; claim 0,94 ms, backlog_counts 7 ms, list_inventory paged auf pkey, Integrity-Verify plant auf chain_position_idx) **und die Typ-Disziplin ist sauber** (100 % timestamptz, kein varchar/json/money; einzige Ausnahme document_date, s. o.). uuidv7/Generated Columns → §4.

## 4) PostgreSQL-18-Potenziale

Baseline live bestätigt: server_version_num=180003 (18.3). Fakten-Check der Feature-Liste: sechs genuine PG18-Features korrekt attribuiert (Skip Scan, uuidv7, io_method/AIO, RETURNING OLD/NEW, NOT NULL NOT VALID, WITHOUT OVERLAPS); Korrekturen: partition-wise join UND aggregate sind beide PG11 (kein PG18-Argument), VIRTUAL-Generated-Spalten sind in PG18 nicht indexierbar (0030-Kommentar dokumentiert das korrekt), CHECK ... NOT VALID gibt es seit 9.2.

**[NIEDRIG] NOT NULL ... NOT VALID als Standard-Konvention für künftige NOT-NULL-Additions übernehmen.**
Die Migrationshistorie validiert bisher durchweg voll: 0034:43-44 `SET NOT NULL` nach Backfill; CHECKs immediate in 0010/0024/0033 (Letzteres Drop+Re-Add unter ACCESS EXCLUSIVE). Bei ~2,6 k Audit-Rows/Tag (~1 M/Jahr) wird das auf den unbounded Tabellen teuer. (Fairerweise: in 0034 selbst war der Mehrpreis marginal, der Backfill schrieb die Tabelle ohnehin um — der Wert ist rein forward-looking.)
*Fix:* In der Migrations-README festschreiben: auf audit_events/jobs/ai_artifacts `ADD CONSTRAINT ... NOT NULL ... NOT VALID` + `VALIDATE CONSTRAINT` (SHARE UPDATE EXCLUSIVE); CHECKs ebenso zweistufig. Kein Retrofit nötig.

**[NIEDRIG] RETURNING OLD/NEW hat genau eine konkrete High-Value-Anwendung: den Cooldown-Upsert new-vs-extended melden lassen (#279-Observability-Lücke).**
upsert_provider_cooldown (db lib.rs:8504-8527) endet in `.execute()` → die einzige Call-Site (worker main.rs:999) kann fresh/extended/no-op nicht unterscheiden. Korrektur zum Sekundärkandidaten: in upsert_inventory_item Change-Detection über old/new `paperless_modified_at` — `ocr_content_hash` wird in dessen SET-Liste nie gesetzt, der Vergleich wäre immer false.
*Fix:* `RETURNING old.cooldown_until, new.cooldown_until, (old.provider_name IS NULL) AS inserted` + Log/Audit-Event, wenn der Code das nächste Mal angefasst wird.

**[INFO] B-Tree Skip Scan: real und live verifiziert** (EXPLAIN „Index Searches: 4" auf jobs_status_run_after_idx ohne Leading-Prädikat) — **aber 0 safe Index-Drops heute**: alle Single-Column-Indizes wurden gegen die Composite-Definitionen re-auditiert (jobs_lease_until/created_at haben keinen nutzbaren Composite; idx_jobs_claim ist partial; audit-Listing braucht die globale created_at-Ordnung). Wert = Design-Regel: vor jedem NEUEN Single-Column-Index auf jobs/runs/review_items/audit_events prüfen, ob ein Composite mit low-cardinality Leading-Column (status/stage/event_type/trigger_tag, alle n_distinct ≤ 31) ihn via Skip Scan bedient.

**[INFO] io_method=worker (PG18-Default) behalten; io_uring wäre unmessbar.** 99,99 % Buffer-Hit-Ratio, nur 1,57 GB Client-Relation-Reads im ~6-Tage-Fenster, DB 239 MB in 2 GB shared_buffers — es existiert praktisch kein uncached Relation-I/O zum Beschleunigen. Optional track_io_timing (CNPG-Parameter) aktivieren. Der reale Optimierungshebel ist die Sync-/Poll-Frequenz (§6.1), nicht I/O.

**[INFO] Partitionierung: nicht adoptiert und bei 239 MB Gesamtgröße / max. 82 k Rows nicht warranted.** Die Batched-Retention (ctid-Batches à 5000, lib.rs:7421-7423) ist das richtige Design; native Partitionierung würde zudem die chain_position-verankerte Audit-Kette und die run_id/job_id-FKs verkomplizieren. Achtung Attribution: partition-wise join+aggregate sind PG11-Features. Revisit-Trigger definieren (z. B. audit_events > 5 GB oder Retention-Batches überholen Autovacuum / ~10× Ingest).

**[INFO] WITHOUT OVERLAPS: kein Use-Case in diesem Schema — explizit skippen.** Es existieren keine periodenmodellierten Daten (grep über alle Migrationen leer); die Exklusivitäts-Invarianten nutzen bereits die richtigen, billigeren Primitives (Partial-Unique pipeline_runs_one_active_per_document_idx, idx_scan=5521 aktiv; skalares cooldown_until pro Provider).

**[INFO] uuidv7() + Generated Columns: bereits vollständig und korrekt adoptiert — keine Migrationsarbeit.** Katalog: einziger id-Default = uuidv7() (15 Spalten; paperless_*-Mirror-Tabellen tragen korrekt Paperless-Integer-IDs ohne Default); attgenerated='s' exakt für jobs.priority/stage_priority + ocr_body_tsv; 0030 dokumentiert die VIRTUAL-Index-Rejection korrekt. Regel beibehalten: STORED für Indexiertes/Read-Hot, VIRTUAL nur für billige nicht-indexierte Ableitungen. Nuance: ocr_body_tsv wird per ts_rank/@@ genutzt, sein GIN-Index hat aber idx_scan=0 (die Chat-Kandidaten-Query OR-t trgm-similarity → Seq-Scan der 6k-Tabelle) — bei aktueller Größe kein Handlungsbedarf.

## 5) Prod-Health

**[HOCH, behoben] Incident: Pipeline-Stall durch ollama.com-Cloud-Weekly-Quota — degradiert ab 08.06. 11:56Z, Totalstall ~32 h (09.06. 05:00Z bis 10.06. 12:56Z; „49 h" des Rohbefunds war überzeichnet, der 08.06. schaffte noch 1.353 Metadata-Jobs), Operator-Remediation 10.06. ~20:00Z.**
1.762 `ai.provider_quota_exhausted`-Events („weekly usage limit", 834 allein am 10.06.); Cloud-Provider entfernt, lokale Verarbeitung läuft wieder (qwen3-paperless:8b + qwen3.5:27b). v1.12.2-Self-Healing bestätigt: Quota-429s verbrauchten keine Attempts, 0 Hard-Fails (vs. 1.589 am 22.05. pre-v1.12.2). Backlog 783 queued, drainiert mit ~15,8 Jobs/h → ~2 Tage.
*Lehre/Fix:* Cloud-Provider draußen lassen oder nur mit expliziten Budgets+Alerts reaktivieren; Alert auf die Quota-Event-Rate — hätte am 08.06. statt 10.06. alarmiert. Direkt gekoppelt an:

**[MITTEL] /metrics weiterhin deaktiviert (ARCHIVIST_METRICS_TOKEN unset) — live verifizierter 503; während des gesamten Stalls existierte keinerlei Scrape-Coverage.**
Deployment-Env vollständig enumeriert (15 Vars, kein envFrom): Token fehlt; curl im API-Pod → 503 „metrics disabled" (main.rs:564-574). Bereits im Re-Audit vom 10.06. geflaggt (#279-#297-Ära), unverändert offen.
*Fix:* Token via Deploy-Repo setzen (digest-pinned GitOps-Flow); Scrape + Alerts auf Queue-Depth, Job-Failure-Rate und Quota-Event-Rate.

**[INFO] Alle 3.516 failed Jobs restlos in 5 bekannte Kategorien zerlegt — kein neuer systemischer Fehlermodus.** 1.589 „Too Many Requests" (22.05., historisch/pre-v1.12.2) + 796 Ops-Dedup + 759 „page not found" (inkl. der 20 aus §1.5) + 363 Custom-Field-Fehler (17.05., eine Stunde) + exakt 9 (nicht 10) Ollama-Connection/Vision-Errors = 3.516. Optional historische Failure-Rows taggen/archivieren, damit Dashboards nur post-v1.12.x zeigen.

**[INFO] Audit-Hash-Kette INTAKT.** 82.017 Rows über Positionen 1..82071; genau zwei benigne chain_position-Lücken (54 Positionen — Rollback-verbrannte Sequenzwerte, beide im turbulenten 10.06.-Fenster); Hash-Verkettung über beide Lücken verifiziert (prev_event_hash-Match). Wichtig: der shipped Verifier (lib.rs:7234-7308) prüft Verkettung, nicht Kontiguität → kein False-Positive; der Kontiguitäts-Caveat gilt nur für externe Ad-hoc-Verifier. *Empfehlung:* Lücken-Semantik dokumentieren, beobachtete Gap-Ranges bei Verifikation protokollieren.

**[INFO] Plattform-Schicht gesund.** api+worker Running, restarts=0; CNPG „healthy" (Replica streaming, Replay-Lag 0,4 ms, ContinuousArchiving=True); Daily-Backups 07.-10.06. auf Objekt-Ebene alle „completed" (das cluster-status-Feld lastSuccessfulBackup=06.06. ist ein Red Herring); Ressourcen winzig (api 12Mi/2m, worker 34Mi/2m); Logs quasi still (0 ERROR, 4 benigne Slow-Statement-WARNs). Pool-Sizing gesund (10+10 App-Connections vs. max_connections=1200, 12 live). Kein schädlicher Bloat: Dead Tuples vernachlässigbar, HOT-Ratio 99,17 % — Preis ist Dauer-Autovacuum (~alle 79 s) auf den Sync-Tabellen, das mit §6.1 verschwindet. Watch-Item: clusterweite transiente Probe-Timeout-Bursts auf ANDEREN Apps (gitlab/harbor/netbox/zitadel) + <node>-Rezidiv — Archivist-Stack unberührt, aber eine längere Episode träfe mehrere Schichten gleichzeitig.

## 6) Weitere Optimierungen

**[HOCH] Full-Inventory-No-op-Rewrite alle ~79 s: 98,5 % aller SQL-Statements und ~9,4 GB WAL/Tag auf einer 239-MB-Datenbank.**
46,57 M von 47,28 M Calls sind unconditional ON-CONFLICT-DO-UPDATE-Upserts (inventory 23,5 M, tags 14,1 M, correspondents 7,2 M, doctypes 1,8 M) mit `last_seen_at=now()` → jede Zeile wird jede Runde physisch neu geschrieben (n_tup_upd 23,5 M bei 5.996 Live-Rows; pg_stat_wal 34 GB/3,6 d bei archive_mode=on). *Evidenz:* lib.rs:2057-2083 (inventory), :1946-1998 (tags/entities); Per-Row-Loop in einer Tx worker main.rs:3883-3943; last_seen_at hat keine Prädikat-Konsumenten (nur Anzeige :4217, Sort-Tiebreak :4523) → Guard verifiziert safe. **Korrektur zum Rohbefund:** Delta-Sync ist nicht nur geplant, sondern shipped (worker main.rs:3861-3880, modified-since-Cursor; API main.rs:6316) und in prod lediglich deaktiviert (`"delta_sync_enabled": false`). Tags/Correspondents/Doctypes (23 M Statements) bleiben aber by design Full-Pass → der Guard ist trotzdem der richtige Fix.
*Fix:* (a) `DO UPDATE ... WHERE (cols) IS DISTINCT FROM (excluded.cols)` auf alle vier Upserts (Bedeutungs-Shift von last_seen_at: last-synced → last-changed, dokumentieren), (b) Multi-Row-unnest-Batching, (c) `delta_sync_enabled=true` schalten. Nebeneffekte: WAL-Volumen kollabiert, Autovacuum-Dauerlauf endet (§5).

**[NIEDRIG] Jeder authentifizierte Request committet ein UPDATE auf sessions.last_seen_at — mit 4,64 ms mean der teuerste Baustein des Auth-Pfads.**
9.567 Calls exakt 1:1 mit dem Session-Lookup (0,31 ms); ungedrosselt in find_session (lib.rs:899-903). Throttle verifiziert safe: last_seen_at speist keine Expiry/Revocation-Logik (Validität nur expires_at/revoked_at/u.enabled, lib.rs:888-891; Anzeige nur in list_sessions).
*Fix:* `... AND (last_seen_at IS NULL OR last_seen_at < now() - interval '60 seconds')` oder clientseitig anhand des bereits gelesenen Werts skippen — eliminiert ~95 % dieser Commits, ~5 ms pro Request.

**[INFO, Exposure widerlegt] Provider-Hygiene: openai/anthropic-Seeds enabled, aber ohne Keys.**
Beide Rows sind unberührte Bootstrap-Seeds (2026-05-14 11:28) mit `secret_reference_id IS NULL` und 0 Artefakten ever — die behauptete stille Billing-Exposure existiert nicht (Calls scheiterten an Auth). Restthema reine Hygiene: enabled-but-nonfunctional Provider plus stale Model-Pins (gpt-5.5 / claude-sonnet-4-20250514).
*Fix:* enabled=false setzen; künftig secret_reference als Voraussetzung für enabled erzwingen; Model-Pins erst bei bewusster Adoption auffrischen — und jeden künftigen Remote-Provider mit dem Quota-/Error-Alerting aus §5.2 koppeln. (Hängt mit der ai_providers-Doppelhaltung aus §3.3 zusammen.)