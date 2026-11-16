/**
 * The import slice's Vortex import. Reads a copy of Vortex's state store (never the live one), so
 * Vortex must be closed: the store's lock is refused with a "close Vortex first" message.
 *
 * The steps are: open the store, choose a Vortex profile that belongs to a game Agora supports,
 * preview what the import would do, then import it. The store and staging folder are only read.
 */

import { useEffect, useRef, useState } from 'react';
import { AlertTriangle, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { BackButton } from '@/components/ui/back-button';
import { Switch } from '@/components/ui/switch';
import { useConfirm } from '@/components/ui/confirm';
import { formatBytes } from '@/components/installed-content/contentTableState';
import { plural } from '@/features/library/format';
import { formatError } from '@/lib/tauri';
import { addGameFolder } from '../discovery/api';
import {
  importVortexProfile,
  scanVortex,
  type VortexPlanDto,
  type VortexRunDto,
  type VortexScanGame,
  type VortexScanProfile,
  type VortexScanReply,
} from './api';
import type { ImportDone, ImportFlowState, ImportStep } from './Mo2ImportFlow';
import { ImportProgressView } from './ImportProgressView';

const STEP_CARD = 'space-y-3 rounded-xl border border-border bg-background/60 p-4';

type ScanState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; reply: VortexScanReply };

type PlanState =
  | { status: 'idle' }
  | { status: 'busy' }
  | { status: 'error'; message: string }
  | { status: 'ready'; plan: VortexPlanDto };

type RunState =
  | { status: 'idle' }
  | { status: 'running'; instanceName: string; startedAt: number }
  | { status: 'error'; message: string; instanceName: string; startedAt: number }
  | { status: 'done'; report: VortexRunDto };

interface VortexImportFlowProps {
  onStateChange?: (state: ImportFlowState) => void;
}

function isLocked(message: string): boolean {
  return /vortex is running|close vortex/i.test(message);
}

/** "3 mods · 2 enabled · plugins.txt", one profile line. */
function profileSummary(profile: VortexScanProfile): string {
  const plugins = profile.pluginsTxt
    ? profile.loadorderTxt
      ? 'plugins.txt and loadorder.txt'
      : 'plugins.txt'
    : profile.loadorderTxt
      ? 'loadorder.txt'
      : 'no plugin list';
  return `${plural(profile.modsTotal, 'mod')} · ${profile.modsEnabled} enabled · ${plugins}`;
}

export function VortexImportFlow({ onStateChange }: VortexImportFlowProps) {
  const { confirm } = useConfirm();
  const [scan, setScan] = useState<ScanState>({ status: 'loading' });
  const [selected, setSelected] = useState<{ game: VortexScanGame; profile: VortexScanProfile } | null>(null);
  const [instanceName, setInstanceName] = useState('');
  const [copySaves, setCopySaves] = useState(false);
  const [addingFolder, setAddingFolder] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);
  const [plan, setPlan] = useState<PlanState>({ status: 'idle' });
  const [run, setRun] = useState<RunState>({ status: 'idle' });
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const load = () => {
    setScan({ status: 'loading' });
    setSelected(null);
    setPlan({ status: 'idle' });
    setRun({ status: 'idle' });
    scanVortex()
      .then((reply) => {
        if (mounted.current) setScan({ status: 'loaded', reply });
      })
      .catch((error: unknown) => {
        if (mounted.current) setScan({ status: 'error', message: formatError(error) });
      });
  };

  useEffect(() => {
    load();
    // Read once when the importer opens; "Look again" reads again.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** Add Vortex's game folder through the same path the Settings folder list uses, then re-read. */
  const addFolder = async (path: string) => {
    setAddingFolder(true);
    setAddError(null);
    try {
      await addGameFolder(path);
      load();
    } catch (error) {
      if (mounted.current) setAddError(formatError(error));
    } finally {
      if (mounted.current) setAddingFolder(false);
    }
  };

  const choose = async (game: VortexScanGame, profile: VortexScanProfile) => {
    if (!game.game) return;
    setSelected({ game, profile });
    setInstanceName('');
    setCopySaves(false);
    setRun({ status: 'idle' });
    setPlan({ status: 'busy' });
    try {
      const reply = await importVortexProfile({
        store: null,
        game: game.game,
        profile: profile.name,
        name: null,
        dryRun: true,
        install: null,
        addFolder: false,
        copySaves,
      });
      if (!mounted.current) return;
      if (reply.kind !== 'plan') throw new Error('The preview did not return a plan.');
      setPlan({ status: 'ready', plan: reply });
    } catch (error) {
      if (mounted.current) setPlan({ status: 'error', message: formatError(error) });
    }
  };

  const importProfile = async (current: VortexPlanDto) => {
    const ok = await confirm({
      title: `Import “${current.profile}” as a new instance?`,
      body: `Agora copies ${plural(current.mods.length, 'mod')} (${formatBytes(current.bytes)}) from Vortex's staging folder and makes an instance named “${current.instanceName}”. Vortex's files are not changed.`,
      confirmLabel: 'Import profile',
    });
    if (!ok) return;
    const newName = instanceName.trim() === '' ? current.instanceName : instanceName.trim();
    const startedAt = Date.now();
    setRun({ status: 'running', instanceName: newName, startedAt });
    try {
      const reply = await importVortexProfile({
        store: null,
        game: current.gameId,
        profile: current.profile,
        name: newName === current.instanceName ? null : newName,
        dryRun: false,
        install: null,
        addFolder: false,
        copySaves,
      });
      if (!mounted.current) return;
      if (reply.kind !== 'run') throw new Error('The import did not return a report.');
      setRun({ status: 'done', report: reply });
    } catch (error) {
      if (mounted.current) {
        setRun({ status: 'error', message: formatError(error), instanceName: newName, startedAt });
      }
    }
  };

  const step: ImportStep =
    run.status === 'done'
      ? 'done'
      : run.status === 'running' || run.status === 'error'
        ? 'importing'
        : plan.status === 'ready' || plan.status === 'busy'
          ? 'preview'
          : 'setup';
  const running = run.status === 'running';
  const report: ImportDone | null =
    run.status === 'done'
      ? { instanceId: run.report.instanceId, instanceName: run.report.instanceName }
      : null;
  useEffect(() => {
    onStateChange?.({ step, running, report });
  }, [onStateChange, step, running, report]);

  if (run.status === 'done') {
    const result = run.report;
    return (
      <section className="space-y-4" data-testid="vortex-run">
        <div className={STEP_CARD}>
          <h3 className="text-base font-semibold">Imported as “{result.instanceName}”</h3>
          <p className="text-sm text-muted-foreground" data-testid="vortex-run-counts">
            {plural(result.storedMods, 'mod')} stored · {plural(result.contentLayers, 'content layer')} ·{' '}
            {result.disabledLayers} disabled
          </p>
          {result.pluginsWritten !== null && (
            <p className="text-sm text-muted-foreground">Plugin list: {result.pluginsWritten} lines written.</p>
          )}
          {result.inisCopied.length > 0 && (
            <p className="text-sm text-muted-foreground">INIs copied: {result.inisCopied.join(', ')}.</p>
          )}
          {result.savesCopied > 0 && (
            <p className="text-sm text-muted-foreground">
              Saves copied: {result.savesCopied}
              {result.savesSkippedExisting > 0 ? ` (${result.savesSkippedExisting} already there, left alone)` : ''}.
            </p>
          )}
        </div>
        {result.duplicates.length > 0 && (
          <div className={STEP_CARD}>
            <h4 className="text-sm font-semibold">Same bytes as a layer already here</h4>
            <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
              {result.duplicates.map((entry) => (
                <li key={entry}>{entry}</li>
              ))}
            </ul>
          </div>
        )}
        {result.pluginFindings.length > 0 && (
          <div className={STEP_CARD} data-testid="vortex-run-findings">
            <h4 className="text-sm font-semibold">The plugin order</h4>
            <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
              {result.pluginFindings.map((finding) => (
                <li key={finding}>{finding}</li>
              ))}
            </ul>
          </div>
        )}
        {result.nextSteps.length > 0 && (
          <div className={STEP_CARD} data-testid="vortex-run-next">
            <h4 className="text-sm font-semibold">Still to do</h4>
            <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
              {result.nextSteps.map((entry) => (
                <li key={entry}>{entry}</li>
              ))}
            </ul>
          </div>
        )}
      </section>
    );
  }

  if (run.status === 'running' || run.status === 'error') {
    return (
      <section data-testid="vortex-importing">
        <ImportProgressView
          title={`Importing “${run.instanceName}”`}
          sourceName="Vortex"
          progress={null}
          startedAt={run.startedAt}
          error={run.status === 'error' ? run.message : null}
          onBack={() => {
            setRun({ status: 'idle' });
            if (selected) void choose(selected.game, selected.profile);
          }}
        />
      </section>
    );
  }

  if (selected && (plan.status === 'ready' || plan.status === 'busy' || plan.status === 'error')) {
    const current = plan.status === 'ready' ? plan.plan : null;
    return (
      <section className="space-y-4" data-testid="vortex-plan-page">
        <div className="flex flex-wrap items-center gap-3">
          <BackButton
            label="Profiles"
            onBack={() => {
              setSelected(null);
              setPlan({ status: 'idle' });
            }}
          />
          <p className="min-w-0 text-sm text-muted-foreground">
            Profile <span className="font-medium text-foreground">“{selected.profile.name}”</span> of{' '}
            {selected.game.gameName ?? selected.game.vortexGame}
          </p>
        </div>
        {plan.status === 'busy' && <p className="text-sm text-muted-foreground">Reading the store…</p>}
        {plan.status === 'error' && (
          <div className={STEP_CARD} data-testid="vortex-plan-error">
            <p className="text-sm text-foreground">{plan.message}</p>
          </div>
        )}
        {current && (
          <div className="space-y-4" data-testid="vortex-plan">
            <div className={STEP_CARD}>
              <h3 className="text-base font-semibold">What an import would do</h3>
              <p className="text-sm text-muted-foreground" data-testid="vortex-plan-mods">
                {plural(current.modsEnabled, 'enabled mod')}, {plural(current.modsDisabled, 'disabled mod')} ·{' '}
                {plural(current.files, 'file')} · {formatBytes(current.bytes)}
              </p>
              <p className="text-sm text-muted-foreground">
                Instance name:{' '}
                <span className="font-medium text-foreground">{instanceName.trim() || current.instanceName}</span>
              </p>
              {current.wouldAddFolder && (
                <p className="text-sm text-muted-foreground">
                  Agora will add the game folder first: {current.installLocation}
                </p>
              )}
              <label className="block space-y-1 text-sm">
                <span className="text-muted-foreground">Name the new instance</span>
                <input
                  className="w-full rounded-md border border-border bg-background px-3 py-1.5 text-sm"
                  value={instanceName}
                  placeholder={current.instanceName}
                  onChange={(event) => setInstanceName(event.target.value)}
                />
              </label>
            </div>

            <div className={STEP_CARD} data-testid="vortex-plan-manifest">
              <h4 className="text-sm font-semibold">Layer order</h4>
              <p className="text-sm text-muted-foreground">
                {current.manifest.found
                  ? `Deployment manifest: ${current.manifest.files} files, ${current.manifest.matched} matched an imported mod.`
                  : `No deployment manifest was found; the order comes from Vortex’s rules alone. Looked in ${current.manifest.searched.join(', ')}.`}
              </p>
              {current.manifest.damaged && (
                <p className="text-sm text-foreground">The manifest could not be used: {current.manifest.damaged}</p>
              )}
              {current.disagreements.length > 0 && (
                <div data-testid="vortex-plan-disagreements">
                  <p className="text-sm font-medium text-foreground">
                    {plural(current.disagreements.length, 'file')} where the manifest and the rules disagree
                  </p>
                  <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
                    {current.disagreements.slice(0, 20).map((entry) => (
                      <li key={entry.file}>
                        <span className="font-mono text-xs text-foreground">{entry.file}</span>: manifest says{' '}
                        {entry.manifestSupplied}, the rules alone would pick {entry.rulesSupply}
                      </li>
                    ))}
                  </ul>
                </div>
              )}
            </div>

            {current.plugins.kept && (
              <div className={STEP_CARD}>
                <h4 className="text-sm font-semibold">Plugin list</h4>
                <p className="text-sm text-muted-foreground">
                  {current.plugins.active} active of {current.plugins.listed} listed.
                </p>
                {current.plugins.notes.map((note) => (
                  <p key={note} className="text-sm text-muted-foreground">
                    {note}
                  </p>
                ))}
              </div>
            )}

            {(current.localSettings || current.localSaves) && (
              <div className={STEP_CARD} data-testid="vortex-plan-profile-files">
                <h4 className="text-sm font-semibold">The profile’s own files</h4>
                {current.localSettings && (
                  <p className="text-sm text-muted-foreground">
                    INIs: {current.inis.length > 0 ? current.inis.join(', ') : 'none found in the profile folder'}
                  </p>
                )}
                {current.localSaves && (
                  <div className="space-y-2">
                    <p className="text-sm text-muted-foreground">
                      Saves: {current.saves.files} file(s), {formatBytes(current.saves.bytes)}. They are copied only when
                      you ask, as MO2’s import does.
                    </p>
                    <label className="flex items-center gap-2 text-sm">
                      <Switch checked={copySaves} onCheckedChange={setCopySaves} />
                      Copy the profile’s saves into the new instance
                    </label>
                  </div>
                )}
              </div>
            )}

            {current.problems.length > 0 && (
              <div className={STEP_CARD} data-testid="vortex-plan-problems">
                <h4 className="text-sm font-semibold">Left out ({current.problems.length})</h4>
                <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
                  {current.problems.map((problem) => (
                    <li key={`${problem.entry}:${problem.reason}`}>
                      {problem.entry}: {problem.reason}
                    </li>
                  ))}
                </ul>
              </div>
            )}

            {current.warnings.length > 0 && (
              <div className={STEP_CARD} data-testid="vortex-plan-warnings">
                <h4 className="text-sm font-semibold">Worth knowing</h4>
                <ul className="list-disc space-y-1 pl-5 text-sm text-muted-foreground">
                  {current.warnings.map((warning) => (
                    <li key={warning}>{warning}</li>
                  ))}
                </ul>
              </div>
            )}

            <div className="flex justify-end">
              <Button type="button" size="sm" onClick={() => void importProfile(current)}>
                Import as a new instance
              </Button>
            </div>
          </div>
        )}
      </section>
    );
  }

  return (
    <section className="space-y-4" data-testid="vortex-scan">
      <div className="flex items-center justify-between gap-3">
        <h3 className="text-base font-semibold">Vortex profiles</h3>
        <Button type="button" variant="outline" size="sm" onClick={load} disabled={scan.status === 'loading'}>
          <RefreshCw aria-hidden="true" />
          Look again
        </Button>
      </div>
      {scan.status === 'loading' && <p className="text-sm text-muted-foreground">Reading the Vortex store…</p>}
      {scan.status === 'error' && (
        <div className={STEP_CARD} data-testid="vortex-scan-error">
          <div className="flex items-start gap-2">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-foreground" aria-hidden="true" />
            <div>
              <p className="text-sm font-medium text-foreground">
                {isLocked(scan.message) ? 'Close Vortex first' : 'The Vortex store could not be read'}
              </p>
              <p className="mt-1 text-sm text-muted-foreground">{scan.message}</p>
            </div>
          </div>
        </div>
      )}
      {scan.status === 'loaded' && (
        <>
          <p className="break-all font-mono text-xs text-muted-foreground">{scan.reply.store}</p>
          {scan.reply.games.length === 0 && (
            <p className="text-sm text-muted-foreground">No games are in this store.</p>
          )}
          {scan.reply.games.map((game) => (
            <div key={game.vortexGame} className={STEP_CARD} data-testid={`vortex-game-${game.vortexGame}`}>
              <div>
                <h4 className="text-sm font-semibold">
                  {game.gameName ?? game.vortexGame}{' '}
                  <span className="font-mono text-xs font-normal text-muted-foreground">{game.vortexGame}</span>
                </h4>
                <p className="mt-1 text-sm text-muted-foreground">
                  {plural(game.mods, 'mod')} · {game.manifestFound ? 'deployment manifest found' : 'no deployment manifest'}
                  {game.staging ? ` · ${game.staging}` : ''}
                </p>
                {game.reason && <p className="mt-1 text-sm text-muted-foreground">{game.reason}</p>}
                {game.detection && <p className="mt-1 text-sm text-muted-foreground">{game.detection}</p>}
                {game.manifestFound ? (
                  <p className="mt-1 text-sm text-muted-foreground">
                    Deployment manifest: {game.manifestFiles} files.
                  </p>
                ) : (
                  <p className="mt-1 text-sm text-muted-foreground">
                    No deployment manifest found; the order comes from Vortex’s rules alone.
                  </p>
                )}
                {game.needsFolderAdd && game.discoveredPath && (
                  <div className="mt-2">
                    <Button
                      type="button"
                      size="sm"
                      disabled={addingFolder}
                      onClick={() => void addFolder(game.discoveredPath as string)}
                    >
                      Add this folder to Agora
                    </Button>
                    {addError && <p className="mt-1 text-sm text-foreground" role="alert">{addError}</p>}
                  </div>
                )}
              </div>
              {game.supported && game.game && game.profiles.length > 0 && (
                <div className="flex flex-wrap gap-2">
                  {game.profiles.map((profile) => (
                    <Button
                      key={profile.id}
                      type="button"
                      variant="outline"
                      size="sm"
                      data-testid={`vortex-profile-${profile.id}`}
                      onClick={() => void choose(game, profile)}
                    >
                      Use “{profile.name}” · {profileSummary(profile)}
                    </Button>
                  ))}
                </div>
              )}
              {game.supported && game.profiles.length === 0 && (
                <p className="text-sm text-muted-foreground">Vortex lists no profile for this game.</p>
              )}
            </div>
          ))}
          {scan.reply.warnings.length > 0 && (
            <div className={STEP_CARD}>
              {scan.reply.warnings.map((warning) => (
                <p key={warning} className="text-sm text-muted-foreground">
                  {warning}
                </p>
              ))}
            </div>
          )}
        </>
      )}
    </section>
  );
}
