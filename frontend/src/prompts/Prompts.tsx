import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { AlertTriangle, Check, GitCompare, History, Info, Play, RotateCcw, Save } from 'lucide-react';
import { api, Prompt, PromptExperiment, PromptTestResponse, PromptUsage, Stage } from '../api/client';
import { promptStageHelp, promptStageOrder, type PromptStageHelp } from '../data/promptHelp';
import { useI18n } from '../i18n/I18nProvider';
import { PageHeader, Status, localizedErrorMessage, run } from '../lib/ui';
import { formatMs } from '../lib/format';

export function Prompts({ setError }: { setError: (error: string | null) => void }) {
  const { t, formatDateTime } = useI18n();
  const [items, setItems] = useState<Prompt[]>([]);
  const [usage, setUsage] = useState<PromptUsage[]>([]);
  const [experiments, setExperiments] = useState<PromptExperiment[]>([]);
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
  const [saving, setSaving] = useState(false);
  const [activating, setActivating] = useState(false);
  const [loading, setLoading] = useState(true);
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
  const promptDirty =
    selectedPrompt == null ||
    editorName.trim() !== selectedPrompt.name ||
    editorContent.trimEnd() !== selectedPrompt.content.trimEnd();
  const stageHelp = promptStageHelp[selectedStage];
  const promptStats = promptTextStats(editorContent);
  const diffStats = comparePrompt && selectedPrompt ? promptDiffStats(comparePrompt.content, editorContent) : null;
  const load = async () => {
    setLoading(true);
    try {
      const [promptData, usageData, experimentData] = await Promise.all([
        api.prompts(),
        api.promptUsage().catch(() => ({ items: [] as PromptUsage[] })),
        api.promptExperiments().catch(() => ({ items: [] as PromptExperiment[] }))
      ]);
      setItems(promptData.items);
      setUsage(usageData.items);
      setExperiments(experimentData.items);
    } catch (err) {
      setError(localizedErrorMessage(err, t, 'Could not load prompts'));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load();
  }, []);

  useEffect(() => {
    if (stagePrompts.length === 0) {
      setSelectedPromptId(null);
      return;
    }
    if (!selectedPromptId || !stagePrompts.some((prompt) => prompt.id === selectedPromptId)) {
      setSelectedPromptId(activePrompt?.id ?? stagePrompts[0].id);
    }
  }, [activePrompt?.id, selectedPromptId, stagePrompts]);

  useEffect(() => {
    if (selectedPrompt) {
      setEditorName(selectedPrompt.name);
      setEditorContent(selectedPrompt.content);
      setActivate(true);
    } else {
      setEditorName('default');
      setEditorContent('');
      setActivate(true);
    }
    setComparePromptId((current) => {
      if (current && stagePrompts.some((prompt) => prompt.id === current && prompt.id !== selectedPrompt?.id)) return current;
      if (activePrompt && activePrompt.id !== selectedPrompt?.id) return activePrompt.id;
      return stagePrompts.find((prompt) => prompt.id !== selectedPrompt?.id)?.id ?? null;
    });
    setTestResult(null);
  }, [activePrompt, selectedPrompt?.id, stagePrompts]);

  return (
    <section className="page">
      <div className="prompt-heading">
        <PageHeader title="Prompt Workbench" />
        <p>
          Review active prompts, tune stage-specific behavior, test outputs, and roll back safely. Edits create a new
          immutable version; older versions remain available.
        </p>
      </div>
      <div className="prompt-workbench">
        <aside className="prompt-stage-rail" aria-label="Prompt stages">
          <header>
            <strong>Pipeline Stages</strong>
            <span>{items.length} versions</span>
          </header>
          {promptStageOrder.map((entry) => {
            const help = promptStageHelp[entry];
            const prompts = items.filter((prompt) => prompt.stage === entry);
            const active = prompts.find((prompt) => prompt.active);
            const usageCount = prompts.reduce((sum, prompt) => sum + (usageByPromptId.get(prompt.id)?.run_count ?? 0), 0);
            return (
              <button
                type="button"
                key={entry}
                className={selectedStage === entry ? 'active' : ''}
                onClick={() => {
                  setSelectedStage(entry);
                  setSelectedPromptId(null);
                  setComparePromptId(null);
                }}
              >
                <span>
                  <strong>{help.label}</strong>
                  <em>{active ? `${active.name} v${active.version}` : 'No prompt yet'}</em>
                </span>
                <small>{prompts.length} versions · {usageCount} runs</small>
              </button>
            );
          })}
        </aside>
        <section className="prompt-editor-card">
          <header className="prompt-card-header">
            <div>
              <div className="prompt-title-row">
                <h3>{stageHelp.label}</h3>
                <PromptInfoTooltip label={`${stageHelp.label} guidance`} help={stageHelp} />
              </div>
              <p>{stageHelp.purpose}</p>
            </div>
            <div className="prompt-header-status">
              {selectedPrompt?.active ? <Status value="active" /> : <Status value="draft" />}
              {promptDirty && <span className="dirty-pill">unsaved edits</span>}
            </div>
          </header>
          {loading ? (
            <div className="empty-state">Loading prompts...</div>
          ) : (
            <>
              <div className="prompt-editor-grid">
                <label>
                  Version
                  <select
                    value={selectedPrompt?.id ?? ''}
                    onChange={(event) => setSelectedPromptId(event.target.value || null)}
                  >
                    {stagePrompts.length === 0 && <option value="">New prompt</option>}
                    {stagePrompts.map((prompt) => (
                      <option key={prompt.id} value={prompt.id}>
                        {promptOptionLabel(prompt)}
                      </option>
                    ))}
                  </select>
                </label>
                <label>
                  Prompt name
                  <input value={editorName} onChange={(event) => setEditorName(event.target.value)} />
                </label>
                <label className="inline prompt-activate-check">
                  <input type="checkbox" checked={activate} onChange={(event) => setActivate(event.target.checked)} />
                  Activate after save
                </label>
              </div>
              <label className="prompt-editor-field">
                Prompt content
                <textarea
                  value={editorContent}
                  onChange={(event) => setEditorContent(event.target.value)}
                  required
                  spellCheck={false}
                />
              </label>
              <div className="prompt-editor-actions">
                <button
                  className="primary-button"
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
                    })
                  }
                >
                  <Save size={16} /> {saving ? 'Saving...' : 'Save New Version'}
                </button>
                <button
                  disabled={!selectedPrompt || !promptDirty}
                  onClick={() => {
                    setEditorName(selectedPrompt?.name ?? 'default');
                    setEditorContent(selectedPrompt?.content ?? '');
                  }}
                >
                  <RotateCcw size={16} /> Reset
                </button>
                <button
                  disabled={activating || !selectedPrompt || selectedPrompt.active}
                  onClick={() =>
                    selectedPrompt &&
                    run(setActivating, setError, async () => {
                      await api.activatePrompt(selectedPrompt.id);
                      await load();
                    })
                  }
                >
                  <Check size={16} /> {activating ? 'Activating...' : 'Activate Selected'}
                </button>
              </div>
              <div className="prompt-stats-grid">
                <PromptStat label="Lines" value={promptStats.lines} />
                <PromptStat label="Words" value={promptStats.words} />
                <PromptStat label="Characters" value={promptStats.characters} />
                <PromptStat label="Runs" value={selectedUsage?.run_count ?? 0} />
              </div>
            </>
          )}
        </section>
        <aside className="prompt-lab-card">
          <section>
            <div className="prompt-section-title">
              <strong>Stage Guide</strong>
              <PromptInfoTooltip label="Prompt editing rules" help={stageHelp} compact />
            </div>
            <p>{stageHelp.expectedOutput}</p>
            <ul>
              {stageHelp.safety.map((item) => <li key={item}>{item}</li>)}
            </ul>
          </section>
          <section>
            <div className="prompt-section-title">
              <strong>Usage</strong>
              <History size={16} />
            </div>
            {selectedUsage ? (
              <dl className="prompt-usage">
                <div><dt>Runs</dt><dd>{selectedUsage.run_count}</dd></div>
                <div><dt>Jobs</dt><dd>{selectedUsage.job_count}</dd></div>
                <div><dt>Last used</dt><dd>{selectedUsage.last_used_at ? formatDateTime(selectedUsage.last_used_at) : '-'}</dd></div>
                <div><dt>Model</dt><dd>{[selectedUsage.last_provider, selectedUsage.last_model].filter(Boolean).join(' / ') || '-'}</dd></div>
                <div><dt>Avg duration</dt><dd>{formatMs(selectedUsage.avg_duration_ms)}</dd></div>
              </dl>
            ) : (
              <p className="field-hint">This prompt version has not been used by a worker run yet.</p>
            )}
          </section>
          <section>
            <div className="prompt-section-title">
              <strong>Version History</strong>
              <span>{stagePrompts.length}</span>
            </div>
            <div className="prompt-version-list">
              {stagePrompts.map((prompt) => (
                <button
                  key={prompt.id}
                  type="button"
                  className={prompt.id === selectedPrompt?.id ? 'active' : ''}
                  onClick={() => setSelectedPromptId(prompt.id)}
                >
                  <span>{prompt.name} v{prompt.version}</span>
                  <small>{prompt.active ? 'active' : formatDateTime(prompt.created_at)}</small>
                </button>
              ))}
              {stagePrompts.length === 0 && <p className="field-hint">No prompt exists for this stage yet.</p>}
            </div>
          </section>
        </aside>
      </div>
      <div className="prompt-lab-grid">
        <section className="prompt-test-card">
          <header className="prompt-section-title">
            <strong>Prompt Test Runner</strong>
            <span>{stageHelp.shortLabel}</span>
          </header>
          <div className="prompt-test-grid">
            <label>
              Test document ID
              <input value={sampleDocumentId} onChange={(event) => setSampleDocumentId(event.target.value)} placeholder="optional" />
            </label>
            <label className="wide">
              Test sample text
              <textarea
                value={sampleText}
                onChange={(event) => setSampleText(event.target.value)}
                placeholder="Optional; overrides document ID for prompt tests."
              />
            </label>
          </div>
          <button
            className="primary-button"
            type="button"
            disabled={testing || !editorContent.trim()}
            onClick={() => run(setTesting, setError, async () => {
              const documentId = sampleDocumentId.trim() ? Number(sampleDocumentId) : null;
              const result = await api.testPrompt({
                stage: selectedStage,
                content: editorContent,
                sample_text: sampleText.trim() || undefined,
                paperless_document_id: documentId && Number.isFinite(documentId) ? documentId : null
              });
              setTestResult(result);
            })}
          >
            <Play size={16} /> {testing ? 'Testing...' : 'Test Current Editor'}
          </button>
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
                <summary>Parsed output</summary>
                <pre>{JSON.stringify(testResult.parsed ?? null, null, 2)}</pre>
              </details>
              <details>
                <summary>Raw model response</summary>
                <pre>{testResult.raw_text}</pre>
              </details>
            </section>
          )}
        </section>
        <section className="prompt-compare-card">
          <header className="prompt-section-title">
            <strong>Version Compare</strong>
            <GitCompare size={16} />
          </header>
          <label>
            Compare against
            <select value={comparePromptId ?? ''} onChange={(event) => setComparePromptId(event.target.value || null)}>
              <option value="">No comparison</option>
              {stagePrompts
                .filter((prompt) => prompt.id !== selectedPrompt?.id)
                .map((prompt) => (
                  <option key={prompt.id} value={prompt.id}>{promptOptionLabel(prompt)}</option>
                ))}
            </select>
          </label>
          {diffStats ? (
            <>
              <div className="prompt-diff-summary">
                <PromptStat label="Changed lines" value={diffStats.changedLines} />
                <PromptStat label="Added lines" value={diffStats.addedLines} />
                <PromptStat label="Removed lines" value={diffStats.removedLines} />
              </div>
              <div className="prompt-diff">
                <div>
                  <strong>{comparePrompt?.name} v{comparePrompt?.version}</strong>
                  <pre>{comparePrompt?.content}</pre>
                </div>
                <div>
                  <strong>Current editor</strong>
                  <pre>{editorContent}</pre>
                </div>
              </div>
            </>
          ) : (
            <p className="field-hint">Select another version to compare it with the current editor content.</p>
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
                  <td>{formatRate(row.approved, row.total)}</td>
                  <td>{row.mean_confidence == null ? '-' : `${(row.mean_confidence * 100).toFixed(1)}%`}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </section>
  );
}

function formatRate(part: number, total: number) {
  if (total <= 0) return '-';
  return `${((part / total) * 100).toFixed(1)}%`;
}

function promptOptionLabel(prompt: Prompt) {
  return `${prompt.name} v${prompt.version}${prompt.active ? ' (active)' : ''}`;
}

function promptTextStats(value: string) {
  const trimmed = value.trim();
  return {
    lines: value ? value.split(/\r?\n/).length : 0,
    words: trimmed ? trimmed.split(/\s+/).length : 0,
    characters: value.length
  };
}

function promptDiffStats(before: string, after: string) {
  const beforeLines = before.split(/\r?\n/);
  const afterLines = after.split(/\r?\n/);
  const max = Math.max(beforeLines.length, afterLines.length);
  let changedLines = 0;
  for (let index = 0; index < max; index += 1) {
    if ((beforeLines[index] ?? '') !== (afterLines[index] ?? '')) changedLines += 1;
  }
  return {
    changedLines,
    addedLines: Math.max(afterLines.length - beforeLines.length, 0),
    removedLines: Math.max(beforeLines.length - afterLines.length, 0)
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
            </>
          )}
        </span>
      )}
    </span>
  );
}
