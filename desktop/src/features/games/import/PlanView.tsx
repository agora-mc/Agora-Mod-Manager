/**
 * The dry run of an import, shown before anything is stored: what would come in, what would be
 * skipped and why, and the button that makes it happen.
 */

import { CircleSlash, FileCode2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { formatBytes } from '@/components/installed-content/contentTableState';
import { plural } from '@/features/library/format';
import type { Mo2PlanDto, PlannedMod, Problem } from './api';
import { plannedModsLabel } from './planRules';

/** The Library card's chip, so a mod reads the same here as it will in the Library. */
const CHIP = 'inline-flex items-center gap-1 rounded-full border border-border px-2 py-0.5 text-xs text-muted-foreground';

/** The mod's place in MO2's priority order (its line in modlist.txt), as a small chip. */
function PriorityChip({ line }: { line: number }) {
  return (
    <span
      className="inline-flex h-6 min-w-8 shrink-0 items-center justify-center rounded-md bg-muted px-1.5 text-xs font-semibold tabular-nums text-muted-foreground"
      title={`Priority ${line} (line ${line} of modlist.txt)`}
      aria-label={`Priority ${line}`}
    >
      {line}
    </span>
  );
}

/** The first letter as a tile, as the Library shows an item without an icon. */
function LetterTile({ name, muted }: { name: string; muted: boolean }) {
  return (
    <span
      aria-hidden="true"
      className={[
        'flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-border bg-muted text-sm font-semibold',
        muted ? 'text-muted-foreground/60' : 'text-muted-foreground',
      ].join(' ')}
    >
      {name.trim().charAt(0).toLocaleUpperCase() || '?'}
    </span>
  );
}

function PlannedModRow({ mod }: { mod: PlannedMod }) {
  return (
    <li className="flex items-center gap-3 px-3 py-2" data-testid="import-plan-mod">
      <PriorityChip line={mod.line} />
      <LetterTile name={mod.folder} muted={!mod.enabled} />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-1.5">
          <span className={['wrap-break-word text-sm font-medium', mod.enabled ? '' : 'text-muted-foreground'].join(' ')}>
            {mod.folder}
          </span>
          {!mod.enabled && <span className={CHIP}>Disabled</span>}
          {mod.pluginFiles.length > 0 && (
            <span className={CHIP} title={mod.pluginFiles.join(', ')}>
              <FileCode2 className="h-3 w-3" aria-hidden="true" />
              {plural(mod.pluginFiles.length, 'plugin')}
            </span>
          )}
        </div>
        <p className="mt-0.5 text-xs text-muted-foreground">
          {plural(mod.files, 'file')}, {formatBytes(mod.bytes)}
          <span aria-hidden="true"> · </span>
          {mod.provenance}
        </p>
      </div>
    </li>
  );
}

/** The same folder, written either way round (case, slashes, a trailing separator). */
function samePath(a: string, b: string): boolean {
  const fold = (path: string) => path.replace(/\//g, '\\').replace(/\\+$/, '').toLowerCase();
  return fold(a) === fold(b);
}

/** Core's reason as a sentence: capitalised, without the "; skipped" the heading already says. */
export function skipReasonText(reason: string): string {
  const trimmed = reason.trim().replace(/;\s*skipped\.?$/i, '').trim();
  return trimmed.charAt(0).toLocaleUpperCase() + trimmed.slice(1);
}

function SkippedRow({ problem }: { problem: Problem }) {
  return (
    <li className="flex items-center gap-3 px-3 py-2">
      {problem.line !== null ? (
        <PriorityChip line={problem.line} />
      ) : (
        <span className="h-6 min-w-8 shrink-0" aria-hidden="true" />
      )}
      <span
        aria-hidden="true"
        className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-dashed border-border text-muted-foreground"
      >
        <CircleSlash className="h-4 w-4" />
      </span>
      <div className="min-w-0 flex-1">
        <p className="wrap-break-word text-sm font-medium">{problem.entry}</p>
        <p className="mt-0.5 text-xs text-muted-foreground">{skipReasonText(problem.reason)}</p>
      </div>
    </li>
  );
}

interface PlanViewProps {
  plan: Mo2PlanDto;
  /** Why the plan cannot be imported, or `null` when it can. Shown above the list. */
  blockReason: string | null;
  busy: boolean;
  onImport: () => void;
}

export function PlanView({ plan, blockReason, busy, onImport }: PlanViewProps) {
  return (
    <section
      aria-labelledby="import-plan-heading"
      className="space-y-4 rounded-xl border border-border bg-background/60 p-4"
      data-testid="import-plan"
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 id="import-plan-heading" className="text-base font-semibold">
            Preview: profile “{plan.profile}”
          </h3>
          <p className="mt-1 text-sm text-muted-foreground">
            Nothing has been stored yet. This is what an import would do.
          </p>
        </div>
        <Badge variant="secondary">{plan.mo2GameName}</Badge>
      </div>

      <dl className="grid gap-3 text-sm sm:grid-cols-3">
        <div>
          <dt className="text-muted-foreground">Mods</dt>
          <dd data-testid="import-plan-mods">{plannedModsLabel(plan)}</dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Files to store</dt>
          <dd>
            {plan.files} files, {formatBytes(plan.bytes)}
          </dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Store and install</dt>
          <dd className="break-all">
            {plan.store}: {plan.installLocation}
            {plan.setupGamePath && !samePath(plan.setupGamePath, plan.installLocation) && (
              <span className="mt-0.5 block text-xs text-muted-foreground" data-testid="import-plan-setup-path">
                Chosen instead of the setup’s own game folder, {plan.setupGamePath}
              </span>
            )}
          </dd>
        </div>
      </dl>

      {blockReason && (
        <p role="alert" className="rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-sm">
          {blockReason}
        </p>
      )}

      {plan.mods.length > 0 && (
        <div>
          <h4 className="text-sm font-medium">Mods, highest priority first</h4>
          <ul
            className="mt-2 max-h-80 divide-y divide-border overflow-y-auto rounded-lg border border-border bg-card"
            aria-label="Mods in this profile"
          >
            {plan.mods.map((mod) => (
              <PlannedModRow key={`${mod.line}-${mod.folder}`} mod={mod} />
            ))}
          </ul>
        </div>
      )}

      {plan.problems.length > 0 && (
        <div>
          <h4 className="text-sm font-medium">Skipped ({plan.problems.length})</h4>
          <p className="mt-1 text-sm text-muted-foreground">
            These are not imported. Nothing else is changed by skipping them.
          </p>
          <ul
            className="mt-2 divide-y divide-border rounded-lg border border-border bg-card"
            aria-label="Skipped"
            data-testid="import-plan-problems"
          >
            {plan.problems.map((problem) => (
              <SkippedRow key={`${problem.line ?? 'none'}-${problem.entry}`} problem={problem} />
            ))}
          </ul>
        </div>
      )}

      {plan.overwrite.present && (
        <div className="text-sm">
          <h4 className="font-medium">Overwrite folder</h4>
          <p className="mt-1 text-muted-foreground">
            {plan.overwrite.files} files, {formatBytes(plan.overwrite.bytes)}.
          </p>
          {plan.overwrite.generated.map((generated) => (
            <p key={generated.name} className="mt-1">
              {plural(generated.files, 'file')} from {generated.name} {generated.files === 1 ? 'becomes' : 'become'} its generated layer. Agora cannot tell what they
              were built from, so the launch check offers a rebuild instead of calling them current.
            </p>
          ))}
          {plan.overwrite.restFiles > 0 && (
            <p className="mt-1">{plural(plan.overwrite.restFiles, 'other file')} {plan.overwrite.restFiles === 1 ? 'becomes' : 'become'} the “MO2 overwrite” layer.</p>
          )}
        </div>
      )}

      <div className="grid gap-3 text-sm sm:grid-cols-2">
        <div>
          <h4 className="font-medium">Plugins</h4>
          <p className="mt-1 text-muted-foreground">
            {plan.plugins.kept
              ? `${plan.plugins.active} of ${plan.plugins.listed} plugins active, ${plan.plugins.locked.length} locked. Load order and locks come from MO2.`
              : 'This game keeps no plugin list, so none is imported.'}
          </p>
          {plan.plugins.notes.map((note) => (
            <p key={note} className="mt-1 text-muted-foreground">{note}</p>
          ))}
        </div>
        <div>
          <h4 className="font-medium">INIs and saves</h4>
          <p className="mt-1 text-muted-foreground">
            {plan.localSettings
              ? `The profile keeps its own INIs: ${plan.inis.join(', ') || 'none found'}.`
              : 'The game’s own INIs are used; the profile keeps none of its own.'}
          </p>
          <p className="mt-1 text-muted-foreground">
            {plan.saves.local
              ? `The profile keeps its own saves: ${plural(plan.saves.files, 'file')}, ${formatBytes(plan.saves.bytes)}. ${plan.saves.note}`
              : 'The profile shares the game’s saves.'}
          </p>
        </div>
      </div>

      {plan.warnings.length > 0 && (
        <ul className="space-y-1 text-sm" aria-label="Warnings">
          {plan.warnings.map((warning) => (
            <li key={warning} className="text-amber-700 dark:text-amber-300">
              {warning}
            </li>
          ))}
        </ul>
      )}

      <div className="flex flex-wrap items-center justify-between gap-3 border-t border-border pt-4">
        <p className="text-sm text-muted-foreground">
          The setup at {plan.setup} is only read. It is not changed.
        </p>
        <Button onClick={onImport} disabled={busy || blockReason !== null}>
          Import as a new instance
        </Button>
      </div>
    </section>
  );
}
