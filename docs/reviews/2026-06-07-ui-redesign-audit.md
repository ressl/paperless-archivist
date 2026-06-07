# UI Redesign Audit — 2026-06-07

Source: 8-lens UI expert workflow (86 findings: 17 high / 43 medium / 26 low).

## Headline problems (root causes)

- No layout token system: :root (app.css:1-17) defines only 16 color tokens and zero scales for spacing, radius, type, or elevation, so all ~190 selectors hardcode px. This is the root cause that makes every other inconsistency unfixable in place.
- No shared Card/Panel primitive: 17 bespoke *-card/*-panel classes (.metric, .chart-panel, .service-card, .cost-total-card, .review-item, .prompt-*-card, .advanced-filter-panel, etc.) each re-declare border+radius+bg+padding. `border: 1px solid var(--line)` alone appears 45x. Same 8px radius cards carry 4 different paddings (16/14/12/4px).
- Card grids invent ~25 different track floors (minmax 130/132/140/160/200/220/260/280/300/360px across app.css:437,552,625,740,1085,1769,2388,2394,2583...) with mixed align-items (stretch vs start), so visually identical tiles render small on one page and large on another - the literal 'boxes sometimes small, sometimes large' complaint.
- Success/summary messages are pumped into the single red error banner (App.tsx:138 `banner error` role=alert; Reviews auto-fix, Dashboard unblock/cooldown call setError on success). A green outcome shown in a red alert box is the single biggest 'looks buggy' signal.
- Broken CSS: undefined tokens var(--paper) (6 uses: app.css:733,770,888,994,1178,1679) and var(--line-strong) (app.css:1053,1978) drop silently, so health-score/recovery chips render transparent; DuplicatesPanel uses classes (.advanced-panel/.duplicates-panel/.duplicates-group) that exist in zero CSS rules, so it renders fully unstyled next to a bordered sibling panel (Inventory.tsx:535,547).
- Illogical structure: the Settings 'Workflow' fieldset (SettingsPage.tsx:621-757) is a ~30-control grab-bag spanning workflow+OCR+tagging+fields+metadata, tiled by auto-fit next to 1-2 control fieldsets, producing ~10x height mismatch in one row. The Dashboard stacks ~8 distinct layout idioms and ~21 competing numbers in one scroll.
- Monolithic, copy-pasted components: SettingsPage 2333 lines / Dashboard 1555 / Inventory 1056, while lib/ui.tsx ships 5 trivial primitives. KPI card markup is duplicated 3x at 3 sizes (28/22/42px), drawers and info-tooltips and sortable-table engines are each reimplemented twice - the architectural cause of the visual inconsistency.
- Misleading and dead interactions: StageMatrix stage names are styled as links but wired to a no-op onStageSelect (Dashboard.tsx:1262); delta colors are not good/bad aware so rising failures show blue/green never red (lib/format.ts:87-91); the hero backlog card is permanently 'warning' toned regardless of value (Dashboard.tsx:1300-1305).

## Proposed IA / navigation redesign

"1) Sidebar: replace the flat 9-item list (App.tsx:106-115) with 3 labelled, fixed-order groups so positions never shift by role - Operations (Dashboard, Inventory, Reviews, Chat), Configuration (Settings, Prompts), Administration (Users, Audit, Debug). Render gated items as stable slots so muscle memory holds across permissions. Add a visually-hidden skip-to-content link as the first focusable element targeting <main>.\n\n2) One route = one H2. PageHeader (lib/ui.tsx:7) keeps emitting a single H2 per route; demote secondary 'titles' (Settings 'Providers', the 3 Users titles, Prompts) to H3 Section headers. Localize the hardcoded 'Prompt Workbench' title and all Prompts strings via t().\n\n3) Settings restructure: split the 'Workflow' grab-bag into honest, comparably-sized panels - Processing & Safety (mode/paused/dry-run/limits), OCR, Tagging, Custom Fields, Metadata Overwrite Rules, Tag Filters. Each becomes its own <Section>+<Card> driven by the shared FormField primitives. Add a sticky action bar with Save + a dirty indicator (compare savedSettings vs settings). Resolve the duplicated workflow controls (Dashboard AutoProcessingCard vs Settings) by making the Dashboard a read+quick-action surface that links to Settings as the single source of truth.\n\n4) Dashboard de-clutter: reduce to a small number of zones with one card system - (a) header row = title + primary actions only (move HealthBadge into the status zone); (b) a tight single-size KPI band (drop the hero/secondary/tertiary 3-tier sizing; use one card with one optional 'featured' treatment and tone/badges for urgency, ordered by actionability with failed/review-queue first); (c) live processing; (d) move ProviderTable, CostPanel and the quality-strip behind the existing compact tabs or a dedicated Analytics sub-view. Make the StageMatrix links navigate to Inventory or render as plain text.\n\n5) Users page: one H2, with Sessions and API Tokens as H3 sub-sections (or sub-tabs) rather than 3 stacked page titles.\n\n6) Inventory: trim the 11-column table by moving the always-on Debug column behind the diagnose drawer/toggle, cap title/tags column widths with truncation+tooltip, and unify the two collapsible panels (advanced filters + duplicates) on the same Card/Section primitive."

## Priority bugs

- **Success summaries render inside the red error/alert banner** — `frontend/src/App.tsx:138; callers Reviews.tsx:86,100, Dashboard.tsx:1176,1187`
  - Add a `tone` prop to the banner (or a separate success/info banner) and route positive results (auto-fix applied/rejected, unblock/cooldown success) to a neutral/green non-alert surface instead of `className="banner error" role="alert"`.
- **Undefined CSS tokens make backgrounds/borders render transparent** — `frontend/src/styles/app.css:733,770,888,994,1178,1679 (--paper); 1053,1978 (--line-strong)`
  - Define --paper and --line-strong in :root (or replace with var(--base)/var(--surface) and var(--line)). Add var(--token, #fallback) fallbacks so future token typos are non-fatal.
- **DuplicatesPanel uses class names that do not exist in any CSS rule** — `frontend/src/inventory/Inventory.tsx:535,547 (.advanced-panel, .duplicates-panel, .duplicates-group)`
  - Reuse the standardized Card/Section primitive (or the existing .advanced-filter-panel chrome) on the duplicates container and style the group list, so both collapsible panels read as the same component with border/padding/background.
- **StageMatrix stage names look clickable but do nothing** — `frontend/src/dashboard/Dashboard.tsx:1262 (onStageSelect={() => undefined}); buttons at 719-722`
  - Wire onStageSelect to onNavigate('inventory', `?stage=...`) like AlertsBar already does, or render the stage label as plain text instead of a .link-button.
- **Delta colors are not good/bad aware - rising failures show blue/green, never red** — `frontend/src/lib/format.ts:87-91; consumed Dashboard.tsx:1304,1307,1315; app.css:467-473`
  - Make tone direction-aware by passing a higherIsBetter flag per metric so good->success/teal and bad->danger/red; ensure rising failures/backlog can read red.
- **Editing a provider's name orphans its typed API key and loaded model list** — `frontend/src/settings/SettingsPage.tsx:929,982,1003-1005,228 (providerSecrets/ollamaModels keyed by provider.name)`
  - Key per-provider transient state by a stable id (index or generated id), not the mutable display name; pass index into secret/model handlers and drop `${provider.name}-${index}` as React key.
- **Hero backlog card is permanently warning-toned regardless of value** — `frontend/src/dashboard/Dashboard.tsx:1300-1305 (tone: 'warning' as const)`
  - Derive hero tone from value/open_backlog_delta (success when low/shrinking, warning/danger when growing) so the color carries meaning.
- **Drawers (role=dialog aria-modal) have no focus management** — `frontend/src/dashboard/Dashboard.tsx:612-655; frontend/src/inventory/Inventory.tsx:880-908`
  - Extract a shared Drawer primitive that moves focus in on open, traps Tab/Shift+Tab, and restores focus to the trigger on close (a useFocusTrap hook), eliminating the duplicated Escape-only handling.
- **Auto-fix confirm count does not match what is executed** — `frontend/src/reviews/Reviews.tsx:71-94`
  - Confirm and execute against the same number - either run against preview.total_pending or word the confirm with items.length (the actual limit sent).
- **Prompts page is hardcoded English while the rest of the app is i18n'd** — `frontend/src/prompts/Prompts.tsx:113-337`
  - Move all Prompts literals (incl. the title 'Prompt Workbench') into the i18n catalog via t(), matching the nav label.

## All findings by category

### bug (21)
- [high] **Stylesheet uses undefined CSS tokens (--paper, --line-strong) so backgrounds/borders render transparent** — `frontend/src/styles/app.css:733,770,888,994,1178,1679 (--paper); :1053,1978 (--line-strong)`
  - Either add --paper and --line-strong to :root, or replace the 8 usages with existing tokens (e.g. var(--base) / var(--surface) for paper, var(--line) for line-strong). Add `var(--token, #fallback)` fallbacks to make future typos non-fatal.
- [high] **Editing a provider's name silently drops its typed API key and loaded model list** — `frontend/src/settings/SettingsPage.tsx:929, 1003-1005, 982, 228`
  - Key per-provider transient state by a stable id (index or a generated provider id) instead of the mutable display name. Pass index into ProviderModelSelect/secret handlers; stop using provider.name as a map key.
- [high] **Stage-matrix stage links are dead (no-op onStageSelect)** — `frontend/src/dashboard/Dashboard.tsx:1262`
  - Wire onStageSelect to onNavigate('inventory', `?stage=${stage}`) (the prop already exists and is used by AlertsBar), or render the stage label as plain text if drill-down isn't implemented. A clickable link that silently no-ops is exactly the 'looks buggy' symptom.
- [high] **Delta colors don't encode good/bad; throughput delta is effectively inverted** — `frontend/src/lib/format.ts:87-91 (consumed Dashboard.tsx:1304,1307,1315)`
  - Make tone direction-aware per metric (pass a `higherIsBetter` flag), mapping good->success/teal and bad->danger/red. As-is the deltas actively mislead the at-a-glance read instead of helping it.
- [high] **DuplicatesPanel uses CSS class names that don't exist — renders as an unstyled, borderless block next to the styled advanced panel** — `frontend/src/inventory/Inventory.tsx:535,547; frontend/src/styles/app.css`
  - Either add `.duplicates-panel`/`.duplicates-group` rules mirroring `.advanced-filter-panel`, or reuse `.advanced-filter-panel` on the duplicates container. Style the group list (remove default ul bullets, add spacing) so both collapsible panels read as the same component.
- [high] **Success messages are rendered inside the red error/alert banner** — `src/App.tsx:137-144; src/reviews/Reviews.tsx:86,100; src/dashboard/Dashboard.tsx:1176,1187`
  - Add a separate success/info banner (or a `tone` prop on the banner) and route these positive summaries to it. A green/neutral toast for 'applied 12, rejected 3' instead of a red role=alert box removes a major 'this looks broken' signal.
- [medium] **Dead ternary for worker_concurrency default value** — `frontend/src/settings/SettingsPage.tsx:1450`
  - Remove the no-op ternary; pass the real env-derived default (or just null with the env defaultLabel already provided at :1452).
- [medium] **Row contentVisibility placeholder height (41px) underestimates real row height (~57px) causing scroll-height/scrollbar jank on a 500-row list** — `frontend/src/inventory/Inventory.tsx:734`
  - Set containIntrinsicSize to the true row height (~57px), or remove the unlabeled icon-button height so text and action rows match. Measure once and hardcode the correct value.
- [medium] **Quick-filter chips silently overwrite advanced-panel status selections (shared arrays)** — `frontend/src/inventory/Inventory.tsx:217-231,340-388`
  - Make chips add/remove only their own value within the array (like toggleStatus at line 583) instead of replacing the array, and derive chip active-state from inclusion rather than exact-equality.
- [medium] **Selection is never cleared when filters change; 'select all' only covers loaded rows, not all matches** — `frontend/src/inventory/Inventory.tsx:279-291,293-305,433-434`
  - Clear/reconcile selection on filter change (drop ids no longer in results), and either rename the header checkbox to 'select loaded' or implement a real select-all-matching with a count badge.
- [medium] **Stage-matrix stage names look clickable but do nothing** — `src/dashboard/Dashboard.tsx:1262 and 719-722`
  - Either wire onStageSelect to onNavigate('inventory', `?...status=...`) like the alert actions do, or render the stage label as plain text (not a link-button) so it doesn't advertise an interaction that never happens.
- [medium] **Auto-fix confirm dialog count does not match what is actually processed** — `src/reviews/Reviews.tsx:71-94`
  - Confirm and execute against the same number. Either pass `preview.total_pending` to autoFixReviewBulk, or word the confirm with the actual limit being sent (items.length).
- [medium] **Users and Audit tables have no empty state and no loading state** — `src/users/Users.tsx:66-122,130-141,171-196; src/audit/Audit.tsx:76-88`
  - Add empty-row placeholders (matching Inventory's `<td colSpan=n className="empty-row">`) and a loading state, consistent with the other tables.
- [medium] **No in-progress indicator while a chat answer is generating** — `src/chat/DocumentChat.tsx:45-67,99-123`
  - Optimistically append the user's message and a pending assistant placeholder, and scroll the .chat-messages container to the bottom on new messages. Without it the app appears frozen after pressing Send.
- [medium] **Inventory row trigger buttons have no busy guard or feedback** — `src/inventory/Inventory.tsx:238-263,752-764`
  - Disable the row action buttons while a trigger is in flight (per-row busy) and surface a transient notice like the bulk rerun does (`setNotice`).
- [low] **First-run wizard reports steps 'complete' that were never actually performed** — `frontend/src/settings/SettingsPage.tsx:1090-1095,1120-1125`
  - Derive the test step from an actual last-test-result state, or relabel it so the checkmark doesn't imply a passing test.
- [low] **Hardware recommendation tooltip only ever shows the first profile** — `frontend/src/settings/SettingsPage.tsx:74,2300-2329`
  - Either select the profile relevant to the current provider/context, or document that only one profile is supported and drop the array indirection.
- [low] **Cross-tab navigation from Dashboard bypasses the stale-error reset** — `src/App.tsx:83-86,152-173`
  - Call setError(null) in the onNavigate handler before setTab, or route it through selectTab.
- [low] **ReviewCard edit state only resets on item.id change, not on patch change** — `src/reviews/Reviews.tsx:166-171,279-301`
  - Key the reset on the patch identity too (e.g. depend on item.suggested_patch) or remount the card with `key={item.id + version}`.
- [low] **Chat session title input frozen to the launch-time locale; not reset after create** — `src/chat/DocumentChat.tsx:12,38-43`
  - Reset sessionTitle after createSession (and consider deriving the default lazily), and clear documentIds if a fresh session is intended.
- [low] **Token expiry input can submit 0 days when cleared** — `src/users/Users.tsx:156-162,148,184`
  - Clamp/validate before submit (fallback to a sensible default when NaN/<1), and consider a dedicated expiry control for rotation rather than reusing the create-form field.

### consistency (26)
- [high] **No shared card/panel primitive — 17+ near-duplicate *-card/*-panel classes** — `frontend/src/styles/app.css (e.g. 441 .metric, 1592 .cost-total-card, 1774 .quality-strip div, 1829 .chart-panel, 2042 .review-item, 2731/2788 .prompt-*-card, 3057 .test-result)`
  - Introduce one `.card` primitive (border + radius + bg + a single padding token) and have feature classes extend it or be replaced by it. This collapses 17 bespoke definitions and guarantees identical box chrome across pages.
- [high] **Same-radius cards use 4 different paddings → visibly mismatched box sizes** — `frontend/src/styles/app.css:444, 1261, 1592, 1774, 1829, 2042, 2794, 3057, 3252`
  - Pick one card padding (e.g. 16px = a `--space-4` token) for full-size cards and one for compact cards, applied via the shared primitive. Remove the ad-hoc 12px/14px variants.
- [high] **Card grids use 6 different auto-fit min-widths → tiles render small here, large there** — `frontend/src/styles/app.css:437, 563, 625, 1085, 1494, 1740, 2388, 2394, 2583, 3254`
  - Define 2-3 standard tile sizes (e.g. compact 140px, default 220px, wide 280px) as named utilities and reuse them, instead of hand-picking a new minmax floor per grid.
- [high] **Panel heights are wildly uneven — the 'Workflow' card is a monolith next to 1-control cards** — `frontend/src/settings/SettingsPage.tsx:461,621-757,758-789,906-920; app.css:2386-2390,3079-3088`
  - Split the giant Workflow fieldset into coherent equal-weight panels (Workflow, OCR, Tagging, Custom Fields, Metadata). Give cards a consistent rhythm (e.g. fixed column flow with `grid-auto-rows` or a 2-column fixed grid) so no single card dominates.
- [high] **Hero 'open backlog' card is hardcoded to warning tone** — `frontend/src/dashboard/Dashboard.tsx:1300-1305`
  - Derive the hero tone from the value/delta (e.g. success when backlog low or shrinking, warning/danger when growing). A permanently-amber hero trains users to ignore the color.
- [high] **No shared card primitive: five grids each invent their own minmax floor and align-items, so 'boxes' are inconsistently sized by design** — `frontend/src/styles/app.css:2386 (.settings-grid minmax 280), :2392 (.provider-list minmax 300), :1085 (.kpi-secondary/.kpi-tertiary minmax 140), :552 (.operations-strip minmax 260+200), :1797 (.dashboard-ops-grid minmax 280-360)`
  - Introduce one card primitive (shared padding 16px, border, radius 8px, min-width:0) and one or two canonical grid utilities (e.g. a 'card-grid' with a single agreed minmax and stretch). Refactor settings-grid, provider-list, operations-strip, kpi tiers, and chart panels onto them so equivalent containers share a size system.
- [medium] **~30 hardcoded hex colors bypass the token palette (incl. --info re-typed as literal)** — `frontend/src/styles/app.css (e.g. 2767 #f7f9fa, 2771 #b7d8cf, 1113 #e0c999, and #28649b)`
  - Promote the recurring tints to tokens (surface-2, border-soft, success/info/warn soft variants) and replace literals with var(); at minimum replace #28649b with var(--info).
- [medium] **Two pill radii (999px and 18px) plus six total radius values, no radius scale** — `frontend/src/styles/app.css (radius 18px at .health-badge ~line region; 999px on chips e.g. .chip-button; radii 2/3/4/6/8px throughout)`
  - Standardize to --radius-sm (4px), --radius-md (8px), --radius-pill (999px). Make .health-badge use the pill token; drop the 3px/18px/6px-vs-8px ambiguity.
- [medium] **Field hints render at inconsistent sizes/colors (bare <small> vs .field-hint)** — `frontend/src/settings/SettingsPage.tsx:468,567,597,614,632,689,707,808,875,919; app.css (no global `small` rule)`
  - Standardize on one hint component/class (.field-hint) for every helper line; drop bare <small>.
- [medium] **Inconsistent button primitives and a wrong icon on this page** — `frontend/src/settings/SettingsPage.tsx:508-510,616-618,838-840,1023-1025,1026-1040,2061-2063`
  - Route all actions through the shared ActionButton/Button primitive with consistent variants; replace UserPlus with a neutral Plus icon for 'Add provider'.
- [medium] **Two stacked KPI rows use different number font sizes** — `frontend/src/styles/app.css:455-458 vs 1090-1092`
  - Pick one stat size for the non-hero KPI cards (or a deliberate two-tier scale applied consistently). Right now the size difference reads as a rendering glitch, not intent.
- [medium] **Card padding and big-number sizes vary widely across sibling dashboard cards** — `frontend/src/styles/app.css:444,655,1068,1612,1775,1882`
  - Introduce spacing + type-scale variables and snap every dashboard card to them (e.g. padding 16px, stat 24px, hero 40px). This single change addresses most of the 'inconsistently sized boxes' perception.
- [medium] **operations-strip: one tall control card beside three short status cards** — `frontend/src/styles/app.css:550-559 (+ Dashboard.tsx:1416-1430)`
  - Either give the autopilot control its own full-width row above the three equal service cards, or move the quota bars / dry-run banner out of the card so the four cards stay close in height.
- [medium] **'neutral' KPI tone has no CSS rule, producing scattershot row backgrounds** — `frontend/src/dashboard/Dashboard.tsx:1306-1317 (CSS app.css:475-494)`
  - Add an explicit neutral style and reserve colored backgrounds for cards that genuinely need an alert state (failed, review). Coloring half the cards for no semantic reason adds noise and weakens hierarchy.
- [medium] **Stage-matrix table forces horizontal scroll in the analytics column** — `frontend/src/styles/app.css:1212-1214`
  - Reduce columns (the avg/p95/throughput trio could collapse) or move the stage matrix to a full-width row so it has room without a horizontal scrollbar.
- [medium] **KPI metric card markup repeated three times, driving inconsistent box sizing** — `frontend/src/dashboard/Dashboard.tsx:1454-1480`
  - Extract a `<MetricCard label value tone delta size>` and map over a single metrics array. Decide deliberately on the size variants instead of having them emerge from three copy-pasted blocks.
- [medium] **Prompts page is hardcoded English while the rest of the app is i18n'd** — `src/prompts/Prompts.tsx:113-337 (e.g. 113,122,143,165,170,184,189,219,228,240,244-247,255,310,314,337)`
  - Move the Prompts literals into the i18n catalog like the rest. Right now switching the UI to German leaves the entire Prompt Workbench in English.
- [low] **Global table min-width: 860px forces horizontal scroll on small tables** — `frontend/src/styles/app.css:92-96`
  - Drop the blanket min-width:860px and apply a min-width only to the wide tables that need it (Inventory), or scope it via a class so small tables size to content.
- [low] **Elevation is ad-hoc: every shadow is a unique one-off, no elevation scale** — `frontend/src/styles/app.css (box-shadow declarations)`
  - Define --shadow-sm/-md/-lg and apply by elevation role (card vs popover vs drawer vs modal).
- [low] **Card font-size jumps are unsystematized (22 vs 28 vs 42px for the same 'big number' role)** — `frontend/src/styles/app.css:457 (.metric strong 28px), 1072 (.metric.hero strong 42px), 1091 (.kpi-tertiary .metric strong 22px)`
  - Tie metric value sizes to a type scale (e.g. --font-size-xl/2xl) and limit the hero variant to one deliberate step up, so KPI tiles in the same grid stay visually coherent.
- [low] **Page titles are inconsistent: some routes emit multiple H2s; Prompts hardcodes an un-localized English title** — `frontend/src/lib/ui.tsx:6-8; frontend/src/settings/SettingsPage.tsx:456 & 922; frontend/src/prompts/Prompts.tsx:113`
  - One H2 per route; demote secondary 'titles' (e.g. 'Providers') to H3 section headers. Localize the Prompts title via t() and keep it consistent with its nav label.
- [low] **Domain string→tone mapping is implemented in three separate places** — `frontend/src/lib/ui.tsx:22-28; frontend/src/inventory/Inventory.tsx:820-833; frontend/src/dashboard/Dashboard.tsx:125-130`
  - Centralize tone tokens and the status/outcome→tone maps in lib/format.ts (or lib/tone.ts) so badges stay visually consistent and the tone set is single-sourced.
- [low] **Inconsistent i18n access: some children take `t` as a prop, siblings call useI18n()** — `frontend/src/inventory/Inventory.tsx:510 (DuplicatesPanel), 582 (AdvancedPanel), 1009 (FieldOutcomeCard) vs 878 (DiagnoseDrawer)`
  - Standardize: non-memoized components should just call useI18n(); reserve passing `t`/handlers for the memoized row/card components where referential stability matters. Removes noise from prop signatures.
- [low] **Same RefreshCw icon used for two different actions (Reload vs Load more)** — `frontend/src/inventory/Inventory.tsx:364,477-482`
  - Give 'load more' a distinct icon (e.g. ChevronDown / Plus) to disambiguate from reload.
- [low] **Audit feedback boxes stack and never dismiss** — `src/audit/Audit.tsx:47-70`
  - Make these dismissible or auto-expiring, consistent with how transient results are surfaced elsewhere.
- [low] **Hardcoded English fragments inside otherwise-translated Dashboard** — `src/dashboard/Dashboard.tsx:947`
  - Move 'requests' into the i18n catalog (e.g. dashboard.cost.requests with interpolation).

### ia (15)
- [high] **'Workflow' fieldset is mislabeled and mixes five unrelated settings domains** — `frontend/src/settings/SettingsPage.tsx:621-757`
  - Regroup by domain into separate fieldsets with accurate legends. This is the core of the 'illogically structured' complaint and the natural decomposition seam.
- [high] **Settings 'Workflow' fieldset is a 30-control grab-bag next to 1-2 control siblings, producing wildly unequal card heights** — `frontend/src/settings/SettingsPage.tsx:621-757 (workflow), :758-789 (completion_tags), :906-920 (ui); CSS frontend/src/styles/app.css:2386-2390 (.settings-grid)`
  - Split the Workflow fieldset into coherent, comparably-sized groups (Processing mode & safety; OCR; Tagging; Custom fields; Metadata overwrite rules; Tag filters) and either give each its own fieldset or move them under tabbed/section navigation. This both fixes the size mismatch and makes the legend honest (a legend 'Workflow' should not own tagging/fields/OCR).
- [medium] **No persistent/sticky Save; primary action is buried under a long scroll with no global dirty indicator** — `frontend/src/settings/SettingsPage.tsx:1022-1042; app.css:1963-1968`
  - Add a sticky action bar with Save and a dirty indicator computed from savedSettings vs settings; keep it visible while scrolling the long form.
- [medium] **Too many competing numbers; weak at-a-glance hierarchy** — `frontend/src/dashboard/Dashboard.tsx:1454-1548`
  - Demote secondary/tertiary detail behind the hero + a tight 4-KPI band, and fold quality metrics into a labeled section. Fewer, ranked numbers is the core fix for 'cluttered + illogically structured'.
- [medium] **Flat 9-item sidebar mixes daily-use and admin views with no grouping or dividers** — `frontend/src/App.tsx:105-117; CSS frontend/src/styles/app.css:222-241 (.sidebar nav)`
  - Group nav into labelled sections, e.g. 'Operations' (Dashboard, Inventory, Reviews, Chat), 'Configuration' (Settings, Prompts), 'Administration' (Users, Audit, Debug), with small section captions or a divider. This is also where the existing `.sidebar` grid-template-rows could host a scrollable nav region.
- [medium] **Nav items change position depending on role, so the menu is unstable between users** — `frontend/src/App.tsx:108-116`
  - Keep stable slot order by rendering disabled/hidden placeholders within fixed groups, or at minimum keep group ordering constant so an item's position never depends on which other items the role can see.
- [medium] **Dashboard stacks ~8 distinct layout systems in one scroll, driving the 'cluttered' feeling** — `frontend/src/dashboard/Dashboard.tsx:1368-1553`
  - Reduce to a small number of zones with one consistent card system, and move secondary analytics (ProviderTable, CostPanel, quality-strip) behind the existing compact tab mechanism (or a dedicated Analytics sub-view) so the default Dashboard shows status + KPIs + live, not everything at once.
- [medium] **KPIs are arbitrarily split into hero/secondary/tertiary tiers with three different sizes and no clear ranking logic** — `frontend/src/dashboard/Dashboard.tsx:1300-1317, 1454-1480; CSS frontend/src/styles/app.css:1059-1092`
  - Use a single KPI card size with one optional 'featured' treatment, and let tone/badges (not three font sizes) signal urgency. If tiers are kept, base them on a defensible rule (e.g. actionable counts first).
- [medium] **Users page crams three distinct resources under three stacked H2 page titles in one tab** — `frontend/src/users/Users.tsx:44, 125, 145`
  - Split into sub-tabs or sub-sections with H3 subheadings under a single H2, or promote Sessions / API Tokens to their own nav entries. Reserve PageHeader/H2 for one title per route.
- [medium] **Workflow controls are duplicated across Dashboard and Settings, splitting one concept across two locations** — `frontend/src/dashboard/Dashboard.tsx:1416-1430 (AutoProcessingCard) and frontend/src/settings/SettingsPage.tsx:621-661 (workflow mode/paused/dry_run/limits)`
  - Pick one home: keep the live mode toggle on the Dashboard as a read-with-quick-action surface, and have it link to Settings for full configuration (or vice-versa). Avoid editing the same fields in two unrelated layouts.
- [medium] **Eleven-column table including an always-on Debug column, min-width:860px, and unbounded title/tags columns drives clutter and horizontal scroll** — `frontend/src/inventory/Inventory.tsx:428-447,748,751; frontend/src/styles/app.css:92-95`
  - Move Debug behind the diagnose drawer or a toggle, cap title/tags column width with truncation + tooltip, and reduce the default column set so the table fits without horizontal scroll.
- [low] **Pause/resume button overlaps with the manual_review mode button** — `frontend/src/dashboard/Dashboard.tsx:417-441`
  - Visually separate the global pause toggle from the mode selector, or use a distinct icon for manual_review, so the two stop-like controls aren't confused.
- [low] **HealthBadge status widget is nested under the page title inside dashboard-heading-main, burying the H2** — `frontend/src/dashboard/Dashboard.tsx:1370-1381; CSS frontend/src/styles/app.css:500-525, 743`
  - Keep the heading row to title + primary actions. Move HealthBadge into the operations-strip or KPI zone where status cards already live, so the page title reads cleanly.
- [low] **Re-run success notice never auto-dismisses and sits inline in the selection bar** — `frontend/src/inventory/Inventory.tsx:301,422`
  - Auto-clear the notice after a few seconds (or on next user interaction) and present it as a transient toast rather than permanent inline text.
- [low] **Numeric search input is silently reinterpreted as an exact ID lookup** — `frontend/src/inventory/Inventory.tsx:204-215`
  - Surface the mode (e.g. a small 'searching by ID' hint or a toggle), or only treat input as an ID when prefixed (#123).

### a11y (8)
- [high] **Modal drawers have no focus management (no focus-in, no trap, no restore)** — `frontend/src/dashboard/Dashboard.tsx:612-655; frontend/src/inventory/Inventory.tsx:880-908`
  - On open, move focus to the drawer (e.g. the close button or an autofocus ref); trap Tab/Shift+Tab within the drawer; on close, restore focus to the trigger. A small useFocusTrap hook reused by both drawers would cover it.
- [medium] **Fixed-column dashboard grids overflow and get clipped between ~900-1180px because breakpoints ignore the 280px sidebar** — `frontend/src/styles/app.css:739-741 (.dashboard-metrics), :1767-1772 (.quality-strip), :376 (.workspace overflow-x:hidden), :2317/2329 (collapse @900)`
  - Use container queries, or raise these collapse breakpoints to account for the sidebar (~1180px), or switch the fixed `repeat(N, minmax(130px,1fr))` grids to `repeat(auto-fit, minmax(130px,1fr))` like .operations-strip already does (line 563).
- [medium] **Compact dashboard tablist is an incomplete ARIA tabs implementation** — `frontend/src/dashboard/Dashboard.tsx:1483-1511, 1517, 1523, 1231`
  - Give each tab an id and aria-controls pointing at its panel; give each panel an id, aria-labelledby, and tabindex={0}; add ArrowLeft/ArrowRight roving-tabindex navigation, or drop the tab/tabpanel roles in favor of plain buttons + region if the full pattern is not worth it.
- [medium] **Chat transcript is not a live region; new assistant replies are not announced** — `frontend/src/chat/DocumentChat.tsx:99-123`
  - Add role="log" aria-live="polite" (or aria-relevant="additions") to .chat-messages; add an explicit aria-label to the icon-only new-chat button (line 82).
- [medium] **--muted text fails AA contrast on table headers and is borderline on the cream base** — `frontend/src/styles/app.css:6 (--muted #687789), :107-110 (th), :9 (--base #f6f3ed)`
  - Darken --muted (e.g. ~#5a6878 gives ~5.3:1 on white) or use --text for table headers. Verify small-text usages reach 4.5:1 against both --surface and --base.
- [medium] **Model-select visible label and accessible name diverge** — `frontend/src/settings/SettingsPage.tsx:527-538,975-986,2111`
  - Make ProviderModelSelect accept a label and render a real <label> wrapping (or htmlFor-linked to) the select, deriving the aria-label from the same visible text.
- [medium] **Row actions are four icon-only buttons with title-only labels and an ambiguous 'full pipeline' icon** — `frontend/src/inventory/Inventory.tsx:752-765`
  - Consolidate into a single overflow/menu button or add concise visible labels; ensure each control keeps an explicit aria-label. Use a clearer icon (or text) for the full re-run.
- [low] **No skip-to-content link; keyboard users tab through the entire sidebar on every page** — `frontend/src/App.tsx:96-235`
  - Add a visually-hidden 'skip to content' link as the first focusable element that targets the <main>/workspace (give it an id), revealed on focus.

### refactor (15)
- [high] **Design system has color tokens only — spacing/radius/type/elevation are all hardcoded px** — `frontend/src/styles/app.css:3-17 (:root) and throughout`
  - Add `--space-*`, `--radius-*`, `--font-size-*`, and `--shadow-*` token scales (e.g. 4/8/12/16/24 spacing; 4/8/999 radius) and migrate hardcoded px to them. This is the structural fix that makes every other inconsistency impossible to reintroduce.
- [high] **SettingsPage renders 8 settings sections as one 460-line JSX block with 53 inline state-spread updaters** — `frontend/src/settings/SettingsPage.tsx:454-921`
  - Split each fieldset into its own component (PaperlessSettings, WorkflowSettings, etc.) under settings/sections/, and introduce typed field primitives (NumberField, CheckboxField, TextField, SelectField) that take a value + onChange, reusing the same pattern already proven in the Tuning section. This removes the deep `{...s, x:{...s.x, y}}` spreads and makes the form scannable.
- [medium] **Duplicate .prompt-compare-card rule split across the file** — `frontend/src/styles/app.css:2731 and 2788-2795`
  - Merge the two .prompt-compare-card blocks (or fold all four prompt cards into the shared card primitive + a layout modifier).
- [medium] **2333-line file mixes one mega-component with ~25 helpers — decompose into per-section panels** — `frontend/src/settings/SettingsPage.tsx:221-1045 (SettingsPage), 1047-2333 (helpers/subcomponents)`
  - Extract each fieldset into a typed <SettingsPanel section={...} value onChange> component (PaperlessPanel, AiDefaultsPanel, WorkflowPanel, etc.) sharing a single Panel primitive so every box has identical chrome and sizing; move the pure helpers (lines ~1158-1913, 1860-2250) into settings/*.ts modules. This both fixes the visual inconsistency (one panel primitive) and shrinks the file.
- [medium] **Modal drawer shell is reimplemented twice instead of being a shared primitive** — `frontend/src/dashboard/Dashboard.tsx:574-657; frontend/src/inventory/Inventory.tsx:878-1007`
  - Extract a `<Drawer open onClose title>` primitive in lib/ui.tsx that owns the backdrop, Escape handling, focus, and close button; both drawers become just their body content. Avoids drift in a11y wiring (one already differs: MaintenanceDrawer early-returns null when closed, Diagnose is gated by the parent).
- [medium] **Info-tooltip popover duplicated between Settings and Prompts** — `frontend/src/settings/SettingsPage.tsx:2275-2333; frontend/src/prompts/Prompts.tsx:491-555`
  - Create a single `<InfoTooltip label content>` (or `usePopover` hook) in lib/. The two call sites pass only their body. Eliminates ~50 lines of duplicated event-listener lifecycle logic that is easy to get subtly wrong.
- [medium] **Sortable-table header logic (sortKey/sortDir/handleSort/arrow) duplicated within Dashboard** — `frontend/src/dashboard/Dashboard.tsx:659-696 (StageMatrix) and 789-851 (ProviderTable)`
  - Extract a `useSortable<T>(rows, defaultKey)` hook returning {sorted, handleSort, arrow} plus a `<SortableTh sortKey/>` component. Collapses two ~60-line sort engines and dozens of repetitive header buttons.
- [medium] **Dashboard.tsx packs 17 components into one 1555-line file** — `frontend/src/dashboard/Dashboard.tsx:199-1069`
  - Move the leaf components to dashboard/components/*.tsx and pure helpers to dashboard/metrics.ts. Improves discoverability and lets the heavy Recharts pieces be reviewed/tested in isolation.
- [medium] **MaintenanceDrawer takes 15 forwarded props (prop-drilling)** — `frontend/src/dashboard/Dashboard.tsx:574-610`
  - Either compose by passing the panels as children, or group the recovery/maintenance/queue concerns into 3 objects. Reduces the call site (Dashboard:1432-1451) from 17 inline props to a handful.
- [low] **Chart colors hardcoded as hex instead of design tokens** — `frontend/src/dashboard/Dashboard.tsx:1237-1291`
  - Reference the existing CSS color tokens (or a shared chart palette constant) so chart colors stay in sync with the rest of the theme.
- [low] **Dead import voided at runtime (formatPercentStandalone)** — `frontend/src/dashboard/Dashboard.tsx:47,1365-1366`
  - Drop the unused import and the void statement.
- [low] **Hand-written memo comparators duplicate the same callback-equality preamble** — `frontend/src/inventory/Inventory.tsx:769-793; frontend/src/reviews/Reviews.tsx:281-301`
  - Since the parents already wrap handlers in useCallback, the callback comparisons are redundant — pass a stable `item` identity (or compare a version/updated_at field) and drop the bespoke comparator, or extract a `shallowEqualExcept` helper. Reduces risk of stale-render bugs.
- [low] **Per-view load/error boilerplate is copy-pasted instead of a shared data hook** — `frontend/src/users/Users.tsx:29-40; frontend/src/audit/Audit.tsx:13-23; frontend/src/chat/DocumentChat.tsx:17-24; frontend/src/prompts/Prompts.tsx:60-80`
  - Introduce a `useAsyncData(fetcher, deps)` / `useResource` hook in lib/ that returns {data, reload, busy} and routes errors to setError once. Removes dozens of duplicated catch arms and the manual `load()`+effect pairing.
- [low] **PAGE_SIZE of 500 loads a very large first page; pagination is manual 'load more' only** — `frontend/src/inventory/Inventory.tsx:17,173,475-484`
  - Lower the default page size (e.g. 50-100) or add real virtualization, and consider numbered pagination so operators can navigate large result sets predictably.
- [low] **Dead `.select-col` class — referenced on th/td but has no CSS, so the checkbox column has no width constraint** — `frontend/src/inventory/Inventory.tsx:429,735; frontend/src/styles/app.css`
  - Add a `.select-col { width: 36px; text-align:center; }` rule (or remove the class) so the selection column is intentionally sized.

### feature (1)
- [medium] **Destructive re-run actions (bulk and per-row) fire immediately with no confirmation** — `frontend/src/inventory/Inventory.tsx:293-305,409-415,752-764`
  - Add a confirmation step for the bulk action (showing the count and stages) and ideally for the full-pipeline single-row action.
