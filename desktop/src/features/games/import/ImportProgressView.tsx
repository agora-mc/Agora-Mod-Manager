/**
 * The "Importing" step of an import from another mod manager: a bar driven by bytes, the counts in
 * words, the mod being stored, the time so far, and what closing means. It reads only the import's
 * progress events, so the Vortex importer can show the same step by naming itself in `sourceName`.
 *
 * Only storing mods can be measured. Every other stage shows a striped bar with no number: a
 * percentage there would be made up.
 */

import { useEffect, useState } from 'react';
import { AlertTriangle, ShieldCheck } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { formatBytes } from '@/components/installed-content/contentTableState';
import { plural } from '@/features/library/format';
import { formatElapsed } from '@/lib/launchProgress';
import type { ImportPhase, ImportProgress } from './api';

const PHASE_WORDS: Record<ImportPhase, string> = {
  storing: 'Copying your mods',
  overwrite: 'Copying the overwrite folder',
  instance: 'Making the new instance',
  layers: 'Adding the mods to the instance',
  finishing: 'Finishing up: plugins, INI files and saves',
};

/** Words for where the import is. `null` progress: no event yet, the setup is being read again. */
export function phaseWords(progress: ImportProgress | null): string {
  if (!progress) return 'Getting ready';
  return PHASE_WORDS[progress.phase] ?? 'Working';
}

/** The bar's whole percent, or `null` when this stage has nothing to measure. */
export function importPercent(progress: ImportProgress | null): number | null {
  if (!progress || progress.phase !== 'storing' || progress.bytesTotal <= 0) return null;
  return Math.min(100, Math.floor((progress.bytesDone / progress.bytesTotal) * 100));
}

interface ImportProgressViewProps {
  /** The step's heading, naming what is being made. */
  title: string;
  /** The program whose setup is being read, for the note that it is only read. */
  sourceName: string;
  progress: ImportProgress | null;
  /** When the import began, in epoch ms, for the time so far. */
  startedAt: number;
  /** Why the import stopped. Shows the failure and the way back in place of the progress. */
  error: string | null;
  onBack: () => void;
}

export function ImportProgressView({ title, sourceName, progress, startedAt, error, onBack }: ImportProgressViewProps) {
  const [now, setNow] = useState(() => Date.now());
  const failed = error !== null;
  useEffect(() => {
    if (failed) return undefined;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [failed]);

  const percent = importPercent(progress);
  const elapsed = formatElapsed(Math.max(0, now - startedAt));
  const words = phaseWords(progress);

  if (error !== null) {
    return (
      <div className="space-y-4" data-testid="import-failed">
        <div role="alert" className="space-y-2 rounded-xl border border-destructive/40 bg-destructive/10 p-5">
          <h3 className="flex items-center gap-2 text-base font-semibold">
            <AlertTriangle className="h-4 w-4 shrink-0 text-destructive" aria-hidden="true" />
            The import did not finish
          </h3>
          <p className="break-words text-sm">{error}</p>
          <p className="text-sm text-muted-foreground">
            {progress ? `It stopped at: ${words.toLowerCase()}. ` : ''}Your {sourceName} setup was only read, so it has not
            changed. Mods already copied stay in your library and are not stored twice if you import again.
          </p>
        </div>
        <Button type="button" variant="outline" onClick={onBack}>
          ← Back to the preview
        </Button>
      </div>
    );
  }

  return (
    <div className="space-y-4" data-testid="import-progress">
      <div className="space-y-4 rounded-xl border border-border bg-background/60 p-5">
        <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
          <div className="min-w-0">
            <h3 className="break-words text-base font-semibold">{title}</h3>
            <p role="status" className="text-sm text-muted-foreground" data-testid="import-phase">{words}…</p>
          </div>
          {percent !== null && (
            <span className="text-3xl font-semibold tabular-nums" data-testid="import-percent">{percent}%</span>
          )}
        </div>

        <div
          className="h-3 overflow-hidden rounded-full bg-muted"
          role="progressbar"
          aria-label="Import progress"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={percent ?? undefined}
          aria-valuetext={percent === null ? words : undefined}
          data-testid="import-bar"
        >
          {percent === null ? (
            <div key="indeterminate" className="agora-progress-indeterminate h-full w-full" data-testid="import-bar-indeterminate" />
          ) : (
            <div key="measured" data-testid="import-bar-fill" className="h-full rounded-full bg-primary transition-[width] duration-500" style={{ width: `${percent}%` }} />
          )}
        </div>

        <p className="min-h-5 truncate text-sm" title={progress?.currentMod ?? undefined} data-testid="import-current-mod">
          {progress?.currentMod && (
            <>
              <span className="text-muted-foreground">Now copying </span>
              <span className="font-medium">{progress.currentMod}</span>
            </>
          )}
        </p>

        <dl className="grid gap-3 sm:grid-cols-3">
          <Stat
            label="Mods"
            value={progress ? `${progress.modsDone} of ${plural(progress.modsTotal, 'mod')}` : '…'}
            testId="import-mods"
          />
          <Stat
            label="Copied"
            value={progress ? `${formatBytes(progress.bytesDone)} of ${formatBytes(progress.bytesTotal)}` : '…'}
            testId="import-bytes"
          />
          <Stat label="Time so far" value={elapsed} testId="import-elapsed" />
        </dl>
      </div>

      <div className="flex items-start gap-3 rounded-xl border border-border p-4 text-sm" data-testid="import-read-only-note">
        <ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-primary" aria-hidden="true" />
        <div className="space-y-1">
          <p className="font-medium">Your {sourceName} setup is only read</p>
          <p className="text-muted-foreground">
            Nothing in it is changed, moved or deleted. A large setup can take many minutes, and you can leave this running
            in the background.
          </p>
          <p className="text-muted-foreground" data-testid="import-quit-note">
            If you quit Agora now, the import stops where it is. Mods already copied stay in your library and aren’t stored
            twice if you import again. An instance left unfinished shows in your list for you to delete.
          </p>
        </div>
      </div>
    </div>
  );
}

function Stat({ label, value, testId }: { label: string; value: string; testId: string }) {
  return (
    <div className="rounded-lg border border-border bg-card/60 px-3 py-2">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="text-sm font-medium tabular-nums" data-testid={testId}>{value}</dd>
    </div>
  );
}
