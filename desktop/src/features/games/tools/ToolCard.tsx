/**
 * One tool in an instance: where it comes from, its output, and the actions on it. Run asks how to capture the
 * output, and asks before anything slower or larger than the default happens (a refused virtual file
 * system, a tool that works on the real install, or a full-copy swap, whose costs are shown first).
 */

import { useState, type ReactNode } from 'react';
import { Wrench } from 'lucide-react';
import { useConfirm } from '@/components/ui/confirm';
import { cn } from '@/lib/utils';
import {
  diffGameTool,
  gameToolSwapCosts,
  removeGameTool,
  rollbackGameTool,
  listToolOutputs,
  listUserToolOutputs,
  runGameTool,
  runUserTool,
  useToolOutput,
  useUserToolOutput,
  type CaptureChoice,
  type DiffReport,
  type RunReport,
  type RunResult,
  type SwapCosts,
  type ToolOffer,
  type ToolOutput,
  type ToolRow,
  type ToolsView,
} from './api';
import {
  CAPTURE_CHOICES,
  messageOf,
  RUN_ONLY_LINE,
  runDetailLines,
  runHeadline,
  shownPaths,
  statusExplanation,
  statusLabel,
  swapCostLines,
} from './toolCopy';

/** A question the run has put to the person. Nothing runs until they answer it. */
type Question =
  | { kind: 'refused'; reason: string; next: 'links' | 'swap' | null }
  | {
      kind: 'swap';
      /** The refusal the swap follows, when it follows one. */
      refusal: string | null;
      /** Why the swap is being offered, in a sentence. */
      why: string;
      /** Its costs, once they are shown. The swap cannot be confirmed before this. */
      costs: SwapCosts | null;
      costsError: string | null;
    };

interface ToolCardProps {
  instanceId: string;
  row: ToolRow;
  busy: boolean;
  setBusy: (busy: boolean) => void;
  onView: (view: ToolsView) => void;
  onChanged: () => void;
  /** Set when the card is one of the person's own tools, run through the user-tool command. */
  userToolId?: string;
  /** Where the tool comes from, in a short line under its name ("From the mod …", "Added by you"). */
  origin?: ReactNode;
  /** The executable's path, shown small. */
  exe?: string | null;
  /** Why the tool cannot run here now (its mod is off or gone). Its output stays manageable. */
  unavailable?: string | null;
  /** Actions on the tool itself rather than its output, such as removing it from the list. */
  headerActions?: ReactNode;
}

/** The status pill's colours: green for current, amber for stale or unknown, red for a conflict. */
function statusTone(status: ToolRow['status']): string {
  switch (status) {
    case 'current':
      return 'border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300';
    case 'stale':
    case 'unknown':
      return 'border-amber-500/50 bg-amber-500/10 text-amber-800 dark:text-amber-300';
    case 'conflict':
      return 'border-destructive/50 bg-destructive/10 text-destructive';
    case null:
      return 'border-border bg-muted text-muted-foreground';
  }
}

const SWAP_WHY_CHOSEN = 'You chose the full-copy swap.';
const SWAP_WHY_REAL_INSTALL = 'This tool works on the game’s real install folder, which the other captures would miss.';

export function ToolCard({
  instanceId,
  row,
  busy,
  setBusy,
  onView,
  onChanged,
  userToolId,
  origin,
  exe,
  unavailable,
  headerActions,
}: ToolCardProps) {
  const { confirm } = useConfirm();
  const [capture, setCapture] = useState<CaptureChoice>('auto');
  const [question, setQuestion] = useState<Question | null>(null);
  const [outcome, setOutcome] = useState<RunReport | null>(null);
  const [offers, setOffers] = useState<ToolOffer[]>([]);
  const [actionError, setActionError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [diff, setDiff] = useState<DiffReport | null>(null);
  const [moveAside, setMoveAside] = useState<{ folder: string; aside: string } | null>(null);
  const [outputs, setOutputs] = useState<ToolOutput[] | null>(null);
  const [outputsError, setOutputsError] = useState<string | null>(null);

  const runOnly = row.runOnly;
  const runLabel = runOnly
    ? `Run ${row.name}`
    : row.status === null ? `Build ${row.name}` : row.status === 'current' ? `Run ${row.name} again` : `Rebuild ${row.name}`;
  // A run-only tool and a tool that is not installed show no build status: there is nothing to build.
  const showStatus = !runOnly && !unavailable;
  const helpText = CAPTURE_CHOICES.find((choice) => choice.value === capture)?.help ?? '';

  const openSwap = (refusal: string | null, why: string, costs: SwapCosts | null = null) => {
    setQuestion({ kind: 'swap', refusal, why, costs, costsError: null });
    if (costs) return;
    gameToolSwapCosts(instanceId)
      .then((loaded) => setQuestion((current) => (current?.kind === 'swap' && current.why === why && current.refusal === refusal
        ? { ...current, costs: loaded }
        : current)))
      .catch((error: unknown) => setQuestion((current) => (current?.kind === 'swap' && current.why === why && current.refusal === refusal
        ? { ...current, costsError: messageOf(error) }
        : current)));
  };

  /** Show a run's answer: a finished run's result and offers, or a question to put to the person. */
  const handle = (result: RunResult) => {
    if (result.status === 'finished') {
      setQuestion(null);
      setOutcome(result.outcome);
      setOffers(result.outcome.offers);
      setNote(null);
      onChanged();
      return;
    }
    if (result.status === 'outputFolderNotEmpty') {
      setOutcome(null);
      setMoveAside({ folder: result.folder, aside: result.aside });
      return;
    }
    if (result.status === 'vfsRefused') {
      setOutcome(null);
      setQuestion({ kind: 'refused', reason: result.reason, next: result.next });
      setOffers(result.offers);
      return;
    }
    setOutcome(null);
    openSwap(null, SWAP_WHY_REAL_INSTALL);
  };

  const run = async (choice: CaptureChoice, options: { refusal?: string; confirmSwap?: boolean; moveAside?: boolean } = {}) => {
    setBusy(true);
    setActionError(null);
    setNote(null);
    try {
      handle(userToolId
        ? await runUserTool(instanceId, userToolId, choice, options)
        : await runGameTool(instanceId, row.toolId, choice, options));
    } catch (error) {
      setActionError(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const startRun = () => {
    setActionError(null);
    if (capture === 'swap') {
      // A swap is never run on the first click: its costs are shown, and the person confirms them.
      setOutcome(null);
      openSwap(null, SWAP_WHY_CHOSEN);
      return;
    }
    void run(capture);
  };

  const confirmSwap = (current: Question & { kind: 'swap' }) => {
    if (!current.costs) return;
    void run('swap', { refusal: current.refusal ?? undefined, confirmSwap: true });
  };

  const confirmMoveAside = () => {
    if (!moveAside) return;
    setMoveAside(null);
    void run(capture, { moveAside: true });
  };

  const loadOutputs = async () => {
    setOutputsError(null);
    try {
      const loaded = userToolId
        ? await listUserToolOutputs(instanceId, userToolId)
        : await listToolOutputs(instanceId, row.toolId);
      setOutputs(loaded);
    } catch (error) {
      setOutputsError(messageOf(error));
    }
  };

  const chooseOutput = async (itemId: string) => {
    setBusy(true);
    setOutputsError(null);
    try {
      const loaded = userToolId
        ? await useUserToolOutput(instanceId, userToolId, itemId)
        : await useToolOutput(instanceId, row.toolId, itemId);
      setOutputs(loaded);
      setNote('The tool’s output now points at the chosen item. The game reads it on its next launch.');
      onChanged();
    } catch (error) {
      setOutputsError(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const rollBack = async () => {
    setBusy(true);
    setActionError(null);
    try {
      const view = await rollbackGameTool(instanceId, row.toolId);
      setNote(`Rolled back. ${row.name}'s output is now generation ${view.tools.find((t) => t.toolId === row.toolId)?.current ?? '?'}, and the game reads it on its next launch.`);
      onView(view);
    } catch (error) {
      setActionError(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    const ok = await confirm({
      title: `Remove ${row.name}’s output?`,
      body: 'Its generations are deleted from this instance. Your mods and the game’s own files are not touched. You can build it again later.',
      confirmLabel: 'Remove output',
      tone: 'danger',
    });
    if (!ok) return;
    setBusy(true);
    setActionError(null);
    try {
      const reply = await removeGameTool(instanceId, row.toolId);
      setNote(`Removed ${row.name}, with ${reply.removedGenerations.length} generation folder(s).`);
      setOutcome(null);
      setDiff(null);
      onView(reply.view);
    } catch (error) {
      setActionError(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const showChanges = async () => {
    setBusy(true);
    setActionError(null);
    try {
      setDiff(await diffGameTool(instanceId, row.toolId));
    } catch (error) {
      setActionError(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const generationLine = row.current === null
    ? 'No output here yet.'
    : `Output: generation ${row.current}${row.previous ? `, with generation ${row.previous} kept to roll back to` : ''}.`;

  return (
    <article
      aria-label={row.name}
      className={cn('space-y-4 rounded-xl border bg-background/40 p-4', unavailable ? 'border-dashed border-border' : 'border-border')}
      data-testid="tool-card"
      data-tool={row.toolId}
    >
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex min-w-0 items-start gap-3">
          <span className={cn('settings-hero-badge', unavailable && 'opacity-50 grayscale')} aria-hidden="true">
            <Wrench className="h-5 w-5" />
          </span>
          <div className="min-w-0 space-y-0.5">
            <div className="flex flex-wrap items-center gap-2">
              <h3 className="text-base font-semibold">{row.name}</h3>
              {showStatus && (
                <span className={cn('rounded-full border px-2.5 py-0.5 text-xs font-semibold', statusTone(row.status))} data-testid="tool-status">
                  {statusLabel(row.status)}
                </span>
              )}
              {unavailable && (
                <span className="rounded-full border border-amber-500/50 bg-amber-500/10 px-2.5 py-0.5 text-xs font-semibold text-amber-800 dark:text-amber-300" data-testid="tool-missing">
                  Not installed
                </span>
              )}
              {!row.declared && (
                <span className="rounded-full border border-border px-2.5 py-0.5 text-xs text-muted-foreground">
                  No longer declared by the game
                </span>
              )}
            </div>
            {origin && <p className="text-sm text-muted-foreground" data-testid="tool-origin">{origin}</p>}
            {exe && <p className="break-all text-xs text-muted-foreground/80" data-testid="tool-exe">{exe}</p>}
          </div>
        </div>
        {headerActions && <div className="flex flex-wrap gap-2">{headerActions}</div>}
      </header>
      {unavailable ? (
        row.current !== null && <p className="text-sm">{generationLine}</p>
      ) : runOnly ? (
        <p className="text-sm text-muted-foreground" data-testid="tool-run-only">
          {RUN_ONLY_LINE}
          {row.current !== null && ` What it wrote is kept as generation ${row.current}.`}
        </p>
      ) : (
        <p className="text-sm">
          <span className="text-muted-foreground">{statusExplanation(row)}</span> {generationLine}
        </p>
      )}
      {unavailable && (
        <p className="rounded-lg border border-amber-500/50 bg-amber-500/10 p-3 text-sm" data-testid="tool-unavailable">{unavailable}</p>
      )}

      <div className="flex flex-wrap items-end gap-3">
        {!unavailable && (
          <>
            {!runOnly && (
              <label className="flex flex-col gap-1 text-sm">
                <span className="font-medium">How to capture the output</span>
                <select
                  value={capture}
                  disabled={busy}
                  onChange={(event) => setCapture(event.target.value as CaptureChoice)}
                  className="rounded-lg border border-border bg-background px-2 py-1.5"
                  aria-describedby={`tool-capture-help-${row.toolId}`}
                >
                  {CAPTURE_CHOICES.map((choice) => (
                    <option key={choice.value} value={choice.value}>{choice.label}</option>
                  ))}
                </select>
                <span id={`tool-capture-help-${row.toolId}`} className="max-w-md text-xs text-muted-foreground">{helpText}</span>
              </label>
            )}
            <button
              type="button"
              disabled={busy}
              onClick={startRun}
              className="rounded-lg bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground shadow-sm disabled:opacity-50"
            >
              {runLabel}
            </button>
          </>
        )}
        {!userToolId && row.previous && (
          <button
            type="button"
            disabled={busy}
            onClick={() => void rollBack()}
            className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:opacity-50"
          >
            Roll back
          </button>
        )}
        {!userToolId && row.previous && (
          <button
            type="button"
            disabled={busy}
            onClick={() => void showChanges()}
            className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:opacity-50"
          >
            Show changes
          </button>
        )}
        {row.outputKind === 'mod' && (
          <button
            type="button"
            disabled={busy}
            onClick={() => void loadOutputs()}
            className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:opacity-50"
          >
            Use an earlier output
          </button>
        )}
        {!userToolId && row.current !== null && (
          <button
            type="button"
            disabled={busy}
            onClick={() => void remove()}
            className="rounded-lg border border-destructive px-3 py-1.5 text-sm font-medium text-destructive hover:bg-destructive/10 disabled:opacity-50"
          >
            Remove output
          </button>
        )}
      </div>

      {actionError && (
        <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">{actionError}</p>
      )}
      {note && <p role="status" className="text-sm text-muted-foreground">{note}</p>}

      {question?.kind === 'refused' && (
        <section role="region" aria-label="The tool could not run its usual way" className="space-y-3 rounded-lg border border-border p-4 text-sm" data-testid="tool-refused">
          <p className="font-medium">The game’s file layer could not start, so nothing ran.</p>
          <p className="text-muted-foreground">{question.reason}</p>
          <div className="flex flex-wrap gap-2">
            {question.next === 'links' && (
              <button
                type="button"
                disabled={busy}
                onClick={() => void run('links', { refusal: question.reason })}
                className="rounded-lg bg-primary px-3 py-1.5 font-medium text-primary-foreground shadow-sm disabled:opacity-50"
              >
                Run with link capture
              </button>
            )}
            {question.next === 'swap' && (
              <button
                type="button"
                disabled={busy}
                onClick={() => openSwap(question.reason, 'The file layer could not start, so the next option is the full-copy swap.')}
                className="rounded-lg bg-primary px-3 py-1.5 font-medium text-primary-foreground shadow-sm disabled:opacity-50"
              >
                Show the full-copy swap
              </button>
            )}
            <button
              type="button"
              onClick={() => setQuestion(null)}
              className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent"
            >
              Not now
            </button>
          </div>
        </section>
      )}

      {question?.kind === 'swap' && (
        <section role="region" aria-label="Full-copy swap" className="space-y-3 rounded-lg border border-border p-4 text-sm" data-testid="tool-swap-confirm">
          <p className="font-medium">{question.why}</p>
          {question.costsError && <p role="alert" className="text-destructive">The swap’s costs could not be read: {question.costsError}</p>}
          {!question.costs && !question.costsError && <p className="text-muted-foreground">Working out what the swap would cost…</p>}
          {question.costs && (
            <ul className="list-disc space-y-1 pl-5" data-testid="tool-swap-costs">
              {swapCostLines(row.name, question.costs).map((line) => <li key={line}>{line}</li>)}
            </ul>
          )}
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              disabled={busy || !question.costs}
              onClick={() => confirmSwap(question)}
              className="rounded-lg bg-primary px-3 py-1.5 font-medium text-primary-foreground shadow-sm disabled:opacity-50"
            >
              Run by full-copy swap
            </button>
            <button
              type="button"
              onClick={() => setQuestion(null)}
              className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent"
            >
              Cancel
            </button>
          </div>
        </section>
      )}

      {outcome && (
        <section role="status" aria-label="Last run" className="space-y-2 rounded-lg border border-border p-4 text-sm" data-testid="tool-outcome">
          <p className="font-medium">{runHeadline(outcome, runOnly)}</p>
          <ul className="list-disc space-y-1 pl-5 text-muted-foreground">
            {runDetailLines(outcome).map((line) => <li key={line}>{line}</li>)}
          </ul>
        </section>
      )}

      {moveAside && (
        <section role="region" aria-label="The output folder already has files" className="space-y-3 rounded-lg border border-border p-4 text-sm" data-testid="tool-output-folder">
          <p className="font-medium">The output folder already has files: {moveAside.folder}</p>
          <p className="text-muted-foreground">
            Nothing was deleted. Move the old files to {moveAside.aside} and run again, or cancel.
          </p>
          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              disabled={busy}
              onClick={confirmMoveAside}
              className="rounded-lg bg-primary px-3 py-1.5 font-medium text-primary-foreground shadow-sm disabled:opacity-50"
            >
              Move it aside and run
            </button>
            <button
              type="button"
              onClick={() => setMoveAside(null)}
              className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent"
            >
              Cancel
            </button>
          </div>
        </section>
      )}

      {outputsError && (
        <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">{outputsError}</p>
      )}
      {outputs && (
        <section aria-label="Earlier outputs" className="space-y-2 rounded-lg border border-border p-4 text-sm" data-testid="tool-outputs">
          <p className="font-medium">Earlier outputs</p>
          {outputs.length === 0 ? (
            <p className="text-muted-foreground">This tool has no earlier outputs yet.</p>
          ) : (
            <ul className="space-y-2">
              {outputs.map((output) => (
                <li key={output.itemId} className="flex flex-wrap items-center gap-2">
                  <span className="break-all">{output.name}</span>
                  <button
                    type="button"
                    disabled={busy}
                    onClick={() => void chooseOutput(output.itemId)}
                    className="rounded-lg border border-border px-2 py-1 text-xs font-medium hover:bg-accent disabled:opacity-50"
                  >
                    Use this output
                  </button>
                </li>
              ))}
            </ul>
          )}
          <button
            type="button"
            onClick={() => setOutputs(null)}
            className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent"
          >
            Close
          </button>
        </section>
      )}

      {offers.length > 0 && (
        <section aria-label="Things to try" className="space-y-2 rounded-lg border border-border p-4 text-sm" data-testid="tool-offers">
          <p className="font-medium">{row.name} has failed more than once in a row. You can try:</p>
          {offers.map((offer) => offer.offer === 'copySwap' ? (
            <div key="copy-swap" className="space-y-2">
              <p>The full-copy swap. It is slower and uses disk space, and it is never run without your say-so.</p>
              <button
                type="button"
                disabled={busy}
                onClick={() => openSwap(null, 'You chose to try the full-copy swap after repeated failures.', offer.costs)}
                className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent disabled:opacity-50"
              >
                Show the full-copy swap
              </button>
            </div>
          ) : (
            <p key="switch-links">
              Linked files. Switch this instance to linked files from its Overview page if you want to try that.
            </p>
          ))}
        </section>
      )}

      {diff && (
        <section aria-label="Changes against the previous generation" className="space-y-2 text-sm" data-testid="tool-diff">
          <p className="font-medium">
            Generation {diff.current} compared with generation {diff.previous}
          </p>
          {diff.added.length + diff.changed.length + diff.removed.length === 0 && (
            <p className="text-muted-foreground">No file differs between the two generations.</p>
          )}
          {[
            { label: 'Added', paths: diff.added },
            { label: 'Changed', paths: diff.changed },
            { label: 'Removed', paths: diff.removed },
          ].map(({ label, paths }) => {
            if (paths.length === 0) return null;
            const { shown, more } = shownPaths(paths);
            return (
              <div key={label}>
                <p className="font-medium">{label} ({paths.length})</p>
                <ul className="list-disc pl-5 text-muted-foreground">
                  {shown.map((path) => <li key={path}>{path}</li>)}
                  {more > 0 && <li>… and {more} more</li>}
                </ul>
              </div>
            );
          })}
        </section>
      )}
    </article>
  );
}
