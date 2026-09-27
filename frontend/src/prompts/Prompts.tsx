import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react';
import { AlertTriangle, Check, GitCompare, History, Info, Play, RotateCcw, Save } from 'lucide-react';
import { api, Prompt, PromptExperiment, PromptTestResponse, PromptUsage, Stage } from '../api/client';
import { promptStageOrder, resolvePromptStageHelp, type PromptStageHelp } from '../data/promptHelp';
import { useI18n, type TFunction } from '../i18n/I18nProvider';
import { Button, PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';
import { ConfirmDialog, useConfirm } from '../lib/ConfirmDialog';
import { useUnsavedChangesGuard } from '../lib/unsavedChanges';
import { formatMs } from '../lib/format';
import { lineDiffStats } from './lineDiff';
import { diffOutputs } from './outputDiff';
import { useResource } from '../lib/useResource';

/** Provider choice for the prompt test runner (#446). */
type TestProviderOption = { name: string; defaultModel: string };

/** One side of the side-by-side version test (#446). */
type CompareOutcome = { ok: true; result: PromptTestResponse } | { ok: false; error: string };

type PendingPromptSelection =
  | { kind: 'stage'; stage: Stage }
  | { kind: 'prompt'; promptId: string | null };

// Stable empty lists so memoised derivations don't recompute before the first load.
const NO_PROMPTS: Prompt[] = [];
const NO_USAGE: PromptUsage[] = [];
const NO_EXPERIMENTS: PromptExperiment[] = [];

export function Prompts({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime, formatPercent } = useI18n();
  const resource = useResource(
    async (signal) => {
      const [promptData, usageData, experimentData] = await Promise.all([
        api.prompts({ signal }),
        // Usage and A/B stats are optional extras; the workbench works without them.
        api.promptUsage({ signal }).catch(() => ({ items: [] as PromptUsage[] })),
        api.promptExperiments({ signal }).catch(() => ({ items: [] as PromptExperiment[] }))
      ]);
      return { items: promptData.items, usage: usageData.items, experiments: experimentData.items };
    },
    [],
    { onError: (err) => setError(localizedErrorMessage(err, t, t('prompts.load_error'))) }
  );
  const load = resource.reload;
  const loading = resource.loading;
  const items = resource.data?.items ?? NO_PROMPTS;
  const usage = resource.data?.usage ?? NO_USAGE;
  const experiments = resource.data?.experiments ?? NO_EXPERIMENTS;
  const [selectedStage, setSelectedStage] = useState<Stage>('ocr');
  const [selectedPromptId, setSelectedPromptId] = useState<string | null>(null);
  const [comparePromptId, setComparePromptId] = useState<string | null>(null);
  const [editorName, setEditorName] = useState('default');
  const [editorContent, setEditorContent] = useState('');
  const [activate, setActivate] = useState(true);
  const [sampleText, setSampleText] = useState('');
  const [sampleDocumentId, setSampleDocumentId] = useState('');
  const [testResult, setTestResult] = useState<PromptTestResponse | null>(null);
  const [testing, setTesting] = useState(false);
  // #446: provider/model for the test runner ('' = stage default) and the
  // side-by-side run of two versions against the same input.
  const [testProvider, setTestProvider] = useState('');
  const [testModel, setTestModel] = useState('');
  const [compareTesting, setCompareTesting] = useState(false);
  const [compareOutcome, setCompareOutcome] = useState<{ left: CompareOutcome; right: CompareOutcome } | null>(null);
  // Enabled text providers from the settings (the page already requires
  // settings:read); without them the runner keeps the stage default.
  const providerResource = useResource<TestProviderOption[]>(
    async (signal) => {
      try {
        const settings = await api.settings({ signal });
        return settings.ai.providers
          .filter((provider) => provider.enabled && provider.kind !== 'mineru')
          .map((provider) => ({ name: provider.name, defaultModel: provider.default_text_model ?? '' }));
      } catch {
        return [];
      }
    },
    []
  );
  const testProviders = providerResource.data ?? [];
  const testProviderDefaultModel = testProviders.find((provider) => provider.name === testProvider)?.defaultModel ?? '';
  const [saving, setSaving] = useState(false);
  const [activating, setActivating] = useState(false);
  const [pendingSelection, setPendingSelection] = useState<PendingPromptSelection | null>(null);
  const { confirm, dialog: confirmDialog } = useConfirm();
  const usageByPromptId = useMemo(() => {
    const byId = new Map<string, PromptUsage>();
    usage.forEach((entry) => byId.set(entry.prompt_id, entry));
    return byId;
  }, [usage]);
  const stagePrompts = useMemo(
    () =>
      items
        .filter((prompt) => prompt.stage === selectedStage)
        .sort((left, right) => {
          if (left.name !== right.name) return left.name.localeCompare(right.name);
          return right.version - left.version;
        }),
    [items, selectedStage]
  );
  const activePrompt = useMemo(
    () =>
      [...stagePrompts]
        .filter((prompt) => prompt.active)
        .sort((left, right) => new Date(right.created_at).getTime() - new Date(left.created_at).getTime())[0] ?? null,
    [stagePrompts]
  );
  const selectedPrompt =
    stagePrompts.find((prompt) => prompt.id === selectedPromptId) ?? activePrompt ?? stagePrompts[0] ?? null;
  const comparePrompt = comparePromptId ? stagePrompts.find((prompt) => prompt.id === comparePromptId) ?? null : null;
  const selectedUsage = selectedPrompt ? usageByPromptId.get(selectedPrompt.id) : undefined;
  // Only the prompt itself (name + content) is a draft. "Activate after save"
  // is a save option, not content: unchecking it must not raise the unsaved
  // changes pill or the discard dialog (#435).
  const promptDirty = selectedPrompt
    ? editorName.trim() !== selectedPrompt.name || editorContent.trimEnd() !== selectedPrompt.content.trimEnd()
    : editorName.trim() !== 'default' || editorContent.trimEnd() !== '';
  // Leaving the Prompts page (sidebar / reload) with a dirty draft asks first (#423).
  useUnsavedChangesGuard(promptDirty);
  const stageHelp = resolvePromptStageHelp(selectedStage, t);
  const promptStats = promptTextStats(editorContent);
  const diffStats = comparePrompt && selectedPrompt ? lineDiffStats(comparePrompt.content, editorContent) : null;

  useEffect(() => {
    if (stagePrompts.length === 0) {
      setSelectedPromptId(null);
      return;
    }
    if (!selectedPromptId || !stagePrompts.some((prompt) => prompt.id === selectedPromptId)) {
      setSelectedPromptId(activePrompt?.id ?? stagePrompts[0].id);
    }
  }, [activePrompt?.id, selectedPromptId, stagePrompts]);

  // The reset effect below also fires on the new array identities every
  // background `load()` produces (e.g. after "Activate selected"), not only on
  // a real selection change. Track which prompt the editor was last synced
  // from plus the live dirty flag (in refs, so they don't retrigger the
  // effect) and skip the reset while the same prompt stays selected with
  // unsaved edits — otherwise activating a version silently discarded the
  // operator's draft (#314).
  const promptDirtyRef = useRef(promptDirty);
  promptDirtyRef.current = promptDirty;
  const editorSyncedPromptIdRef = useRef<string | null | undefined>(undefined);

  useEffect(() => {
    const selectedId = selectedPrompt?.id ?? null;
    const selectionChanged = editorSyncedPromptIdRef.current !== selectedId;
    if (!selectionChanged && promptDirtyRef.current) return;
    editorSyncedPromptIdRef.current = selectedId;
    if (selectedPrompt) {
      setEditorName(selectedPrompt.name);
      setEditorContent(selectedPrompt.content);
    } else {
      setEditorName('default');
      setEditorContent('');
    }
    // A background reload of the same prompt keeps the operator's save option;
    // only a different selection starts again from "activate after save".
    if (selectionChanged) setActivate(true);
    setComparePromptId((current) => {
      if (current && stagePrompts.some((prompt) => prompt.id === current && prompt.id !== selectedPrompt?.id)) return current;
      if (activePrompt && activePrompt.id !== selectedPrompt?.id) return activePrompt.id;
      return stagePrompts.find((prompt) => prompt.id !== selectedPrompt?.id)?.id ?? null;
    });
    setTestResult(null);
    setCompareOutcome(null);
  }, [activePrompt, selectedPrompt?.id, stagePrompts]);

  // Same stage, input, provider and model for the single and the compare run.
  const buildTestInput = (content: string) => {
    const documentId = sampleDocumentId.trim() ? Number(sampleDocumentId) : null;
    return {
      stage: selectedStage,
      content,
      sample_text: sampleText.trim() || undefined,
      paperless_document_id: documentId && Number.isFinite(documentId) ? documentId : null,
      ...(testProvider ? { provider_name: testProvider } : {}),
      ...(testModel.trim() ? { model: testModel.trim() } : {})
    };
  };

  // #446: run the compared version and the editor one after the other (a
  // local model serves one request at a time) and keep each side's outcome.
  const runComparison = async () => {
    if (!comparePrompt) return;
    setCompareTesting(true);
    setCompareOutcome(null);
    const runOne = async (content: string): Promise<CompareOutcome> => {
      try {
        return { ok: true, result: await api.testPrompt(buildTestInput(content)) };
      } catch (err) {
        return { ok: false, error: localizedErrorMessage(err, t) };
      }
    };
    try {
      const left = await runOne(comparePrompt.content);
      const right = await runOne(editorContent);
      setCompareOutcome({ left, right });
    } finally {
      setCompareTesting(false);
    }
  };

  const applySelection = (selection: PendingPromptSelection) => {
    if (selection.kind === 'stage') {
      setSelectedStage(selection.stage);
      setSelectedPromptId(null);
      setComparePromptId(null);
      return;
    }
    setSelectedPromptId(selection.promptId);
  };

  const requestSelection = (selection: PendingPromptSelection) => {
    const unchanged =
      selection.kind === 'stage'
        ? selection.stage === selectedStage
        : selection.promptId === (selectedPrompt?.id ?? null);
    if (unchanged) return;
    if (promptDirty) {
      setPendingSelection(selection);
      return;
    }
    applySelection(selection);
  };

  const cancelPendingSelection = useCallback(() => setPendingSelection(null), []);

  const discardDraftAndSwitch = () => {
    const selection = pendingSelection;
    setPendingSelection(null);
    if (selection) applySelection(selection);
  };

  return (
    <section className="page">
      <div className="prompt-heading">
        <PageHeader title={t('prompts.workbench_title')} />
        <p>{t('prompts.workbench_intro')}</p>
      </div>
      <div className="prompt-workbench">
        <aside className="prompt-stage-rail" aria-label={t('prompts.stages_aria')}>
          <header>
            <strong>{t('prompts.pipeline_stages')}</strong>
            <span>{t('prompts.versions_count', { count: items.length })}</span>
          </header>
          {promptStageOrder.map((entry) => {
            const help = resolvePromptStageHelp(entry, t);
            const prompts = items.filter((prompt) => prompt.stage === entry);
            const active = prompts.find((prompt) => prompt.active);
            const usageCount = prompts.reduce((sum, prompt) => sum + (usageByPromptId.get(prompt.id)?.run_count ?? 0), 0);
            return (
              <button
                type="button"
                key={entry}
                className={selectedStage === entry ? 'active' : ''}
                onClick={() => requestSelection({ kind: 'stage', stage: entry })}
              >
                <span>
                  <strong>{help.label}</strong>
                  <em>{active ? `${active.name} v${active.version}` : t('prompts.no_prompt_yet')}</em>
                </span>
                <small>{t('prompts.stage_summary', { versions: prompts.length, runs: usageCount })}</small>
              </button>
            );
          })}
        </aside>
        <section className="prompt-editor-card">
          <header className="prompt-card-header">
            <div>
              <div className="prompt-title-row">
                <h3>{stageHelp.label}</h3>
                <PromptInfoTooltip label={t('prompts.stage_guidance', { stage: stageHelp.label })} help={stageHelp} />
              </div>
              <p>{stageHelp.purpose}</p>
            </div>
            <div className="prompt-header-status">
              {selectedPrompt?.active ? <Status value="active" /> : <Status value="draft" />}
              {promptDirty && <span className="dirty-pill">{t('prompts.unsaved_edits')}</span>}
            </div>
          </header>
          {loading ? (
            <div className="empty-state">{t('prompts.loading')}</div>
          ) : (
            <>
              <div className="prompt-editor-grid">
                <label>
                  {t('prompts.version')}
                  <select
                    value={selectedPrompt?.id ?? ''}
                    onChange={(event) =>
                      requestSelection({ kind: 'prompt', promptId: event.target.value || null })
                    }
                  >
                    {stagePrompts.length === 0 && <option value="">{t('prompts.new_prompt')}</option>}
                    {stagePrompts.map((prompt) => (
                      <option key={prompt.id} value={prompt.id}>
                        {promptOptionLabel(prompt, t)}
                      </option>
                    ))}
                  </select>
                </label>
                <label>
                  {t('prompts.name')}
                  <input value={editorName} onChange={(event) => setEditorName(event.target.value)} />
                </label>
                <label className="inline prompt-activate-check">
                  <input type="checkbox" checked={activate} onChange={(event) => setActivate(event.target.checked)} />
                  {t('prompts.activate_after_save')}
                </label>
              </div>
              <label className="prompt-editor-field">
                {t('prompts.content')}
                <textarea
                  value={editorContent}
                  onChange={(event) => setEditorContent(event.target.value)}
                  required
                  spellCheck={false}
                />
              </label>
              <div className="prompt-editor-actions">
                <Button
                  variant="primary"
                  icon={<Save size={16} />}
                  disabled={saving || !editorName.trim() || !editorContent.trim()}
                  onClick={() =>
                    run(setSaving, setError, async () => {
                      const result = await api.createPrompt({
                        stage: selectedStage,
                        name: editorName.trim(),
                        content: editorContent.trimEnd(),
                        output_schema: selectedPrompt?.output_schema,
                        activate
                      });
                      await load();
                      setSelectedPromptId(result.id);
                    }, t)
                  }
                >
                  {saving ? t('prompts.saving') : t('prompts.save_new_version')}
                </Button>
                <Button
                  variant="secondary"
                  icon={<RotateCcw size={16} />}
                  disabled={!selectedPrompt || !promptDirty}
                  onClick={() => {
                    setEditorName(selectedPrompt?.name ?? 'default');
                    setEditorContent(selectedPrompt?.content ?? '');
                    setActivate(true);
                  }}
                >
                  {t('prompts.reset')}
                </Button>
                <Button
                  variant="secondary"
                  icon={<Check size={16} />}
                  disabled={activating || !selectedPrompt || selectedPrompt.active}
                  onClick={async () => {
                    if (!selectedPrompt) return;
                    // Activation switches the prompt every new run of this
                    // stage uses, so confirm with the exact version (#417).
                    const confirmed = await confirm({
                      title: t('prompts.activate_confirm.title'),
                      description: t('prompts.activate_confirm.description', {
                        name: selectedPrompt.name,
                        version: selectedPrompt.version,
                        stage: stageHelp.label
                      }),
                      confirmLabel: t('prompts.activate_selected'),
                      tone: 'default',
                      details: activePrompt
                        ? t('prompts.activate_confirm.replaces', { name: activePrompt.name, version: activePrompt.version })
                        : undefined
                    });
                    if (!confirmed) return;
                    await run(setActivating, setError, async () => {
                      await api.activatePrompt(selectedPrompt.id);
                      await load();
                    }, t);
                  }}
                >
                  {activating ? t('prompts.activating') : t('prompts.activate_selected')}
                </Button>
              </div>
              <div className="prompt-stats-grid">
                <PromptStat label={t('prompts.stat_lines')} value={promptStats.lines} />
                <PromptStat label={t('prompts.stat_words')} value={promptStats.words} />
                <PromptStat label={t('prompts.stat_characters')} value={promptStats.characters} />
                <PromptStat label={t('prompts.runs')} value={selectedUsage?.run_count ?? 0} />
              </div>
            </>
          )}
        </section>
        <aside className="prompt-lab-card">
          <section>
            <div className="prompt-section-title">
              <strong>{t('prompts.stage_guide')}</strong>
              <PromptInfoTooltip label={t('prompts.editing_rules')} help={stageHelp} compact />
            </div>
            <p>{stageHelp.expectedOutput}</p>
            <ul>
              {stageHelp.safety.map((item) => <li key={item}>{item}</li>)}
            </ul>
            <strong>{t('prompts.stage_examples')}</strong>
            <ul>
              {stageHelp.examples.map((item) => <li key={item}>{item}</li>)}
            </ul>
          </section>
          <section>
            <div className="prompt-section-title">
              <strong>{t('prompts.usage')}</strong>
              <History size={16} />
            </div>
            {selectedUsage ? (
              <dl className="prompt-usage">
                <div><dt>{t('prompts.runs')}</dt><dd>{selectedUsage.run_count}</dd></div>
                <div><dt>{t('prompts.usage_jobs')}</dt><dd>{selectedUsage.job_count}</dd></div>
                <div><dt>{t('prompts.usage_last_used')}</dt><dd>{selectedUsage.last_used_at ? formatDateTime(selectedUsage.last_used_at) : '-'}</dd></div>
                <div><dt>{t('prompts.usage_model')}</dt><dd>{[selectedUsage.last_provider, selectedUsage.last_model].filter(Boolean).join(' / ') || '-'}</dd></div>
                <div><dt>{t('prompts.usage_avg_duration')}</dt><dd>{formatMs(selectedUsage.avg_duration_ms)}</dd></div>
              </dl>
            ) : (
              <p className="field-hint">{t('prompts.usage_empty')}</p>
            )}
          </section>
          <section>
            <div className="prompt-section-title">
              <strong>{t('prompts.version_history')}</strong>
              <span>{stagePrompts.length}</span>
            </div>
            <div className="prompt-version-list">
              {stagePrompts.map((prompt) => (
                <button
                  key={prompt.id}
                  type="button"
                  className={prompt.id === selectedPrompt?.id ? 'active' : ''}
                  onClick={() => requestSelection({ kind: 'prompt', promptId: prompt.id })}
                >
                  <span>{prompt.name} v{prompt.version}</span>
                  <small>{prompt.active ? t('prompts.active_marker') : formatDateTime(prompt.created_at)}</small>
                </button>
              ))}
              {stagePrompts.length === 0 && <p className="field-hint">{t('prompts.stage_empty')}</p>}
            </div>
          </section>
        </aside>
      </div>
      <div className="prompt-lab-grid">
        <section className="prompt-test-card">
          <header className="prompt-section-title">
            <strong>{t('prompts.test_runner')}</strong>
            <span>{stageHelp.shortLabel}</span>
          </header>
          <div className="prompt-test-grid">
            <label>
              {t('prompts.test_document_id')}
              <input value={sampleDocumentId} onChange={(event) => setSampleDocumentId(event.target.value)} placeholder={t('prompts.optional')} />
            </label>
            <label>
              {t('prompts.test_provider')}
              <select value={testProvider} onChange={(event) => setTestProvider(event.target.value)}>
                <option value="">{t('prompts.test_provider_default')}</option>
                {testProviders.map((provider) => (
                  <option key={provider.name} value={provider.name}>
                    {provider.name}
                  </option>
                ))}
              </select>
            </label>
            <label>
              {t('prompts.test_model')}
              <input
                value={testModel}
                maxLength={200}
                onChange={(event) => setTestModel(event.target.value)}
                placeholder={testProviderDefaultModel || t('prompts.test_model_placeholder')}
              />
            </label>
            <label className="wide">
              {t('prompts.test_sample_text')}
              <textarea
                value={sampleText}
                onChange={(event) => setSampleText(event.target.value)}
                placeholder={t('prompts.test_sample_placeholder')}
              />
            </label>
          </div>
          <Button
            variant="primary"
            type="button"
            icon={<Play size={16} />}
            disabled={testing || !editorContent.trim()}
            onClick={() => run(setTesting, setError, async () => {
              const result = await api.testPrompt(buildTestInput(editorContent));
              setTestResult(result);
            }, t)}
          >
            {testing ? t('prompts.testing') : t('prompts.test_current_editor')}
          </Button>
          {testResult && (
            <section className="test-result">
              <header>
                <strong>{testResult.provider} / {testResult.model}</strong>
                <span>{formatMs(testResult.duration_ms)}</span>
                <Status value={testResult.validation_errors.length === 0 ? 'valid' : 'failed'} />
              </header>
              {testResult.validation_errors.length > 0 && (
                <ul>
                  {testResult.validation_errors.map((error) => <li key={error}>{error}</li>)}
                </ul>
              )}
              {testResult.warnings.length > 0 && (
                <ul className="prompt-warning-list">
                  {testResult.warnings.map((warning) => <li key={warning}><AlertTriangle size={14} /> {warning}</li>)}
                </ul>
              )}
              <details open>
                <summary>{t('prompts.parsed_output')}</summary>
                <pre>{JSON.stringify(testResult.parsed ?? null, null, 2)}</pre>
              </details>
              <details>
                <summary>{t('prompts.raw_response')}</summary>
                <pre>{testResult.raw_text}</pre>
              </details>
            </section>
          )}
        </section>
        <section className="prompt-compare-card">
          <header className="prompt-section-title">
            <strong>{t('prompts.version_compare')}</strong>
            <GitCompare size={16} />
          </header>
          <label>
            {t('prompts.compare_against')}
            <select
              value={comparePromptId ?? ''}
              onChange={(event) => {
                setComparePromptId(event.target.value || null);
                setCompareOutcome(null);
              }}
            >
              <option value="">{t('prompts.no_comparison')}</option>
              {stagePrompts
                .filter((prompt) => prompt.id !== selectedPrompt?.id)
                .map((prompt) => (
                  <option key={prompt.id} value={prompt.id}>{promptOptionLabel(prompt, t)}</option>
                ))}
            </select>
          </label>
          {diffStats ? (
            <>
              <div className="prompt-diff-summary">
                <PromptStat label={t('prompts.diff_changed')} value={diffStats.changedLines} />
                <PromptStat label={t('prompts.diff_added')} value={diffStats.addedLines} />
                <PromptStat label={t('prompts.diff_removed')} value={diffStats.removedLines} />
              </div>
              <div className="prompt-diff">
                <div>
                  <strong>{comparePrompt?.name} v{comparePrompt?.version}</strong>
                  <pre>{comparePrompt?.content}</pre>
                </div>
                <div>
                  <strong>{t('prompts.current_editor')}</strong>
                  <pre>{editorContent}</pre>
                </div>
              </div>
              <p className="field-hint">{t('prompts.compare_hint')}</p>
              <Button
                variant="secondary"
                type="button"
                icon={<Play size={16} />}
                disabled={compareTesting || testing || !editorContent.trim() || !comparePrompt}
                onClick={() => void runComparison()}
              >
                {compareTesting ? t('prompts.compare_running') : t('prompts.compare_run')}
              </Button>
              {compareOutcome && comparePrompt && (
                <PromptCompareResults
                  leftLabel={`${comparePrompt.name} v${comparePrompt.version}`}
                  rightLabel={t('prompts.current_editor')}
                  outcome={compareOutcome}
                  t={t}
                />
              )}
            </>
          ) : (
            <p className="field-hint">{t('prompts.compare_empty')}</p>
          )}
        </section>
      </div>
      <section className="prompt-experiment-card">
        <header className="prompt-section-title">
          <strong>{t('prompts.ab.title')}</strong>
          <GitCompare size={16} />
        </header>
        <p className="field-hint">{t('prompts.ab.description')}</p>
        {experiments.length === 0 ? (
          <p className="field-hint">{t('prompts.ab.empty')}</p>
        ) : (
          <table className="prompt-experiment-table">
            <thead>
              <tr>
                <th>{t('prompts.ab.group')}</th>
                <th>{t('prompts.ab.total')}</th>
                <th>{t('prompts.ab.approved')}</th>
                <th>{t('prompts.ab.rejected')}</th>
                <th>{t('prompts.ab.edited')}</th>
                <th>{t('prompts.ab.applied')}</th>
                <th>{t('prompts.ab.approval_rate')}</th>
                <th>{t('prompts.ab.mean_confidence')}</th>
              </tr>
            </thead>
            <tbody>
              {experiments.map((row) => (
                <tr key={row.group}>
                  <td><strong>{row.group}</strong></td>
                  <td>{row.total}</td>
                  <td>{row.approved}</td>
                  <td>{row.rejected}</td>
                  <td>{row.edited}</td>
                  <td>{row.applied}</td>
                  <td>{row.total > 0 ? formatPercent(row.approved / row.total, 1) : '-'}</td>
                  <td>{row.mean_confidence == null ? '-' : formatPercent(row.mean_confidence, 1)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      {pendingSelection && (
        <ConfirmDialog
          title={t('prompts.draft_dialog.title')}
          description={t('prompts.draft_dialog.description')}
          cancelLabel={t('prompts.draft_dialog.cancel')}
          confirmLabel={t('prompts.draft_dialog.discard')}
          onCancel={cancelPendingSelection}
          onConfirm={discardDraftAndSwitch}
        />
      )}
      {confirmDialog}
    </section>
  );
}

/** Side-by-side results of one version comparison with a field diff (#446). */
function PromptCompareResults({
  leftLabel,
  rightLabel,
  outcome,
  t
}: {
  leftLabel: string;
  rightLabel: string;
  outcome: { left: CompareOutcome; right: CompareOutcome };
  t: TFunction;
}) {
  const rows =
    outcome.left.ok && outcome.right.ok ? diffOutputs(outcome.left.result.parsed, outcome.right.result.parsed) : null;
  const side = (label: string, entry: CompareOutcome) => (
    <div>
      <strong>{label}</strong>
      {entry.ok ? (
        <>
          <small>
            {entry.result.provider} / {entry.result.model} · {formatMs(entry.result.duration_ms)}
          </small>
          <Status value={entry.result.validation_errors.length === 0 ? 'valid' : 'failed'} />
          <pre>{JSON.stringify(entry.result.parsed ?? null, null, 2)}</pre>
        </>
      ) : (
        <p role="alert">{t('prompts.compare_failed', { error: entry.error })}</p>
      )}
    </div>
  );
  return (
    <section className="prompt-compare-results" aria-label={t('prompts.compare_results')}>
      <div className="prompt-diff">
        {side(leftLabel, outcome.left)}
        {side(rightLabel, outcome.right)}
      </div>
      {rows && rows.length === 0 && <p className="field-hint" role="status">{t('prompts.compare_identical')}</p>}
      {rows && rows.length > 0 && (
        <table className="prompt-experiment-table">
          <caption>{t('prompts.compare_differences', { count: rows.length })}</caption>
          <thead>
            <tr>
              <th scope="col">{t('prompts.compare_field')}</th>
              <th scope="col">{leftLabel}</th>
              <th scope="col">{rightLabel}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.path}>
                <td><code>{row.path}</code></td>
                <td>{row.left ?? '—'}</td>
                <td>{row.right ?? '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}

function promptOptionLabel(prompt: Prompt, t: TFunction) {
  return `${prompt.name} v${prompt.version}${prompt.active ? ` (${t('prompts.active_marker')})` : ''}`;
}

function promptTextStats(value: string) {
  const trimmed = value.trim();
  return {
    lines: value ? value.split(/\r?\n/).length : 0,
    words: trimmed ? trimmed.split(/\s+/).length : 0,
    characters: value.length
  };
}

function PromptStat({ label, value }: { label: string; value: number }) {
  return (
    <div className="prompt-stat">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function PromptInfoTooltip({
  label,
  help,
  compact
}: {
  label: string;
  help: PromptStageHelp;
  compact?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const tooltipId = useId();
  const shellRef = useRef<HTMLSpanElement | null>(null);

  useEffect(() => {
    if (!open) return undefined;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false);
    };
    const onPointerDown = (event: MouseEvent | TouchEvent) => {
      if (shellRef.current && !shellRef.current.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener('keydown', onKeyDown);
    document.addEventListener('mousedown', onPointerDown);
    document.addEventListener('touchstart', onPointerDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      document.removeEventListener('mousedown', onPointerDown);
      document.removeEventListener('touchstart', onPointerDown);
    };
  }, [open]);

  return (
    <span
      className="tooltip-shell prompt-tooltip-shell"
      ref={shellRef}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        type="button"
        className="info-button"
        aria-label={label}
        aria-describedby={open ? tooltipId : undefined}
        onClick={() => setOpen((value) => !value)}
        onFocus={() => setOpen(true)}
      >
        <Info size={16} />
      </button>
      {open && (
        <span className={`prompt-info-tooltip${compact ? ' compact' : ''}`} id={tooltipId} role="tooltip">
          <strong>{help.label}</strong>
          <span>{help.purpose}</span>
          {!compact && (
            <>
              <em>{help.expectedOutput}</em>
              <ul>
                {help.safety.map((item) => <li key={item}>{item}</li>)}
              </ul>
              <ul>
                {help.examples.map((item) => <li key={item}>{item}</li>)}
              </ul>
            </>
          )}
        </span>
      )}
    </span>
  );
}
