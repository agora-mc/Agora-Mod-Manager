/**
 * The import slice's Mod Organizer 2 import, opened as a dialog from a game's Instances page
 * ("Import instances"), as Minecraft's "Import Instances" is.
 *
 * Find a setup (the usual places and the drives, or a folder the user picks), choose a profile,
 * preview what an import would do, then import it with progress. The setup is only read.
 */

import { useEffect, useRef, useState } from 'react';
import type { UnlistenFn } from '@tauri-apps/api/event';
import { ArrowRight, CheckCircle2, FolderOpen, RefreshCw } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { BackButton } from '@/components/ui/back-button';
import { useConfirm } from '@/components/ui/confirm';
import { Switch } from '@/components/ui/switch';
import { ChoiceCards } from '@/components/instance-kit';
import { formatBytes } from '@/components/installed-content/contentTableState';
import { plural } from '@/features/library/format';
import { openGameSettings } from '@/lib/settingsSection';
import { formatError, pickDirectory } from '@/lib/tauri';
import {
  importMo2Profile,
  listenImportProgress,
  NO_INSTALL_CODE,
  scanMo2Setups,
  type ChooseInstallDto,
  type ImportProgress,
  type Mo2PlanDto,
  type Mo2RunDto,
  type SetupFound,
} from './api';
import { loadGameSettingsOrDefaults } from '../settings/gameSettings';
import { isGamesError, type InstallSummary } from '../shared/types';
import { storeName } from '../shared/stores';
import { planBlockReason } from './planRules';
import { PlanView } from './PlanView';

/** One step's panel inside the dialog: a quiet card on the dialog's own surface. */
const STEP_CARD = 'space-y-3 rounded-xl border border-border bg-background/60 p-4';

type ScanState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; setups: SetupFound[]; warnings: string[] };

type PlanState =
  | { status: 'idle' }
  | { status: 'busy' }
  | { status: 'error'; message: string; noInstall: boolean }
  | { status: 'ready'; plan: Mo2PlanDto };

type RunState =
  | { status: 'idle' }
  | { status: 'running'; progress: ImportProgress | null }
  | { status: 'error'; message: string }
  | { status: 'done'; report: Mo2RunDto };

/** Where the import has got to, for the dialog's step row and footer. */
export interface ImportFlowState {
  step: ImportStep;
  /** An import is copying files; the dialog must not close. */
  running: boolean;
  /** The finished import's report, once there is one. */
  report: Mo2RunDto | null;
}

export type ImportStep = 'setup' | 'profile' | 'preview' | 'done';

/** The step page the dialog shows. Each step is its own page, and Back returns to the last one. */
type Page = Exclude<ImportStep, 'done'>;

/** "3 profiles · 12 mods · found in the usual place", with what could not be read said plainly. */
export function setupSummary(setup: SetupFound): string {
  const profiles = setup.profiles.length === 0 ? 'No profiles' : plural(setup.profiles.length, 'profile');
  const mods = setup.modCount === null ? 'mods folder unreadable' : plural(setup.modCount, 'mod');
  return `${profiles} · ${mods} · found ${setup.foundBy}`;
}

/** "Steam 1.6.1170", an install's name in the choice of installs. */
function installTitle(install: InstallSummary): string {
  const version = install.runtime.status === 'identified' ? install.runtime.runtime.version : install.storeVersion;
  return [storeName(install.store), version].filter(Boolean).join(' ');
}

interface Mo2ImportFlowProps {
  gameId: string;
  onStateChange?: (state: ImportFlowState) => void;
}

/** Every step of the import, without a frame of its own. */
export function Mo2ImportFlow({ gameId, onStateChange }: Mo2ImportFlowProps) {
  const { confirm } = useConfirm();
  const [scan, setScan]= useState<ScanState>({ status: 'loading' });
  const [pickError, setPickError] = useState<string | null>(null);
  const [chosen, setChosen] = useState<SetupFound | null>(null);
  const [profile, setProfile] = useState('');
  const [instanceName, setInstanceName] = useState('');
  const [copySaves, setCopySaves] = useState(false);
  // The default for this game, applied to every setup the user chooses.
  const [copySavesDefault, setCopySavesDefault] = useState(false);
  const [plan, setPlan] = useState<PlanState>({ status: 'idle' });
  const [run, setRun] = useState<RunState>({ status: 'idle' });
  const [page, setPage] = useState<Page>('setup');
  // When the setup's own game folder is not an install Agora found: the installs to choose from,
  // and the one the user chose. Nothing is chosen for them.
  const [installChoice, setInstallChoice] = useState<ChooseInstallDto | null>(null);
  const [install, setInstall] = useState<string | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const loadSetups = () => {
    setScan({ status: 'loading' });
    scanMo2Setups()
      .then((reply) => {
        if (mounted.current) setScan({ status: 'loaded', setups: reply.setups, warnings: reply.warnings });
      })
      .catch((error: unknown) => {
        if (mounted.current) setScan({ status: 'error', message: formatError(error) });
      });
  };

  useEffect(() => {
    let cancelled = false;
    loadGameSettingsOrDefaults(gameId)
      .then((settings) => {
        if (cancelled) return;
        const wanted = settings.importCopySaves === 'on';
        setCopySavesDefault(wanted);
        setCopySaves(wanted);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [gameId]);

  useEffect(() => {
    loadSetups();
    // Looked up when the dialog opens; "Look again" looks again.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const choose = (setup: SetupFound) => {
    setChosen(setup);
    setProfile(setup.profiles[0] ?? '');
    setInstanceName('');
    setCopySaves(copySavesDefault);
    setPlan({ status: 'idle' });
    setRun({ status: 'idle' });
    setInstallChoice(null);
    setInstall(null);
    setPage('profile');
  };

  const pickFolder = async () => {
    setPickError(null);
    try {
      // The picker answers null only when the user cancels; a failed picker rejects and is shown.
      const folder = await pickDirectory('Choose the folder that holds ModOrganizer.ini');
      if (!folder) return;
      const reply = await scanMo2Setups(folder);
      const [only] = reply.setups;
      if (!only) {
        setPickError('No setup was found in that folder.');
        return;
      }
      choose(only);
    } catch (error) {
      setPickError(formatError(error));
    }
  };

  const request = (dryRun: boolean, onto: string | null = install) => ({
    setupPath: chosen?.iniPath ?? '',
    profile,
    name: instanceName.trim() === '' ? null : instanceName.trim(),
    copySaves,
    dryRun,
    install: onto,
  });

  /** Preview onto `onto`, the install the user chose if any. The preview is a step page of its own. */
  const previewImport = async (onto: string | null = install) => {
    setPlan({ status: 'busy' });
    try {
      const reply = await importMo2Profile(request(true, onto));
      if (!mounted.current) return;
      if (reply.kind === 'choose_install') {
        setInstallChoice(reply);
        setInstall(null);
        setPlan({ status: 'idle' });
        setPage('preview');
        return;
      }
      if (reply.kind !== 'plan') throw new Error('The preview did not return a plan.');
      setPlan({ status: 'ready', plan: reply });
      setPage('preview');
    } catch (error) {
      if (mounted.current) {
        setPlan({
          status: 'error',
          message: formatError(error),
          noInstall: isGamesError(error) && error.code === NO_INSTALL_CODE,
        });
      }
    }
  };

  const chooseInstall = (installId: string) => {
    setInstall(installId);
    void previewImport(installId);
  };

  /** A change on the profile page makes the last preview, and the install chosen for it, stale. */
  const resetPreview = () => {
    setPlan({ status: 'idle' });
    setRun({ status: 'idle' });
    setInstallChoice(null);
    setInstall(null);
  };

  const runImport = async (current: Mo2PlanDto) => {
    const ok = await confirm({
      title: `Import “${current.profile}” as a new instance?`,
      body: `Agora copies ${current.mods.length} mods (${formatBytes(current.bytes)}) into its content store and makes an instance named “${current.instanceName}”. The Mod Organizer 2 setup is not changed.`,
      confirmLabel: 'Import profile',
    });
    if (!ok) return;
    setRun({ status: 'running', progress: null });
    let unlisten: UnlistenFn | null = null;
    try {
      unlisten = await listenImportProgress((progress) => {
        if (mounted.current) setRun({ status: 'running', progress });
      });
      const reply = await importMo2Profile(request(false));
      if (reply.kind !== 'run') throw new Error('The import did not return a report.');
      if (mounted.current) setRun({ status: 'done', report: reply });
    } catch (error) {
      if (mounted.current) setRun({ status: 'error', message: formatError(error) });
    } finally {
      unlisten?.();
    }
  };

  // Each step is a fresh page: start it at its top, not where the last one was scrolled to.
  const pageTop = useRef<HTMLElement | null>(null);
  useEffect(() => {
    pageTop.current?.scrollIntoView?.({ block: 'start' });
  }, [page]);

  const step: ImportStep = run.status === 'done' ? 'done' : page;
  const running = run.status === 'running';
  const report = run.status === 'done' ? run.report : null;
  useEffect(() => {
    onStateChange?.({ step, running, report });
  }, [onStateChange, step, running, report]);

  // A finished import is the whole story now: only its report is shown, so the earlier steps
  // cannot be run again by mistake.
  if (run.status === 'done') {
    return (
      <section ref={pageTop} className="scroll-mt-4 space-y-5" data-testid="import-page">
        <RunSummary report={run.report} />
      </section>
    );
  }

  if (page === 'preview' && chosen) {
    const readyPlan = plan.status === 'ready' ? plan.plan : null;
    return (
      <section ref={pageTop} className="scroll-mt-4 space-y-5" data-testid="import-page">
        <div className="flex flex-wrap items-center gap-3">
          <BackButton label="Profile" onBack={() => setPage('profile')} />
          <p className="min-w-0 text-sm text-muted-foreground">
            Profile <span className="font-medium text-foreground">“{profile}”</span>
            {chosen.gameName ? ` of ${chosen.gameName}` : ''}
          </p>
        </div>

        {installChoice && (
          <div className={STEP_CARD} data-testid="import-install-choice">
            <div>
              <h3 className="text-base font-semibold">Choose the install to import onto</h3>
              <p className="mt-1 text-sm text-muted-foreground">
                This setup expects {installChoice.gameName} in{' '}
                <span className="break-all font-mono text-xs text-foreground">{installChoice.setupGamePath}</span>, but
                Agora did not find the game there. It found the installs below. The new instance runs on the one you
                choose, and the setup is still only read.
              </p>
            </div>
            <ChoiceCards
              label="Install to import onto"
              value={install ?? ''}
              disabled={plan.status === 'busy' || running}
              onChange={chooseInstall}
              options={installChoice.installs.map((entry) => ({
                value: entry.installId,
                label: installTitle(entry),
                description: <span className="break-all">{entry.location}</span>,
              }))}
            />
          </div>
        )}

        {plan.status === 'busy' && (
          <p role="status" className="text-sm text-muted-foreground">Reading the profile…</p>
        )}

        {plan.status === 'error' && <PreviewError message={plan.message} noInstall={plan.noInstall} />}

        {readyPlan && (
          <PlanView
            plan={readyPlan}
            blockReason={planBlockReason(readyPlan, gameId)}
            busy={running}
            onImport={() => runImport(readyPlan)}
          />
        )}

        {run.status === 'running' && (
          <p role="status" className="rounded-xl border border-primary/40 bg-primary/5 p-4 text-sm" data-testid="import-progress">
            {run.progress
              ? `Storing mods: ${run.progress.modsDone} of ${run.progress.modsTotal}, ${formatBytes(run.progress.bytesDone)} of ${formatBytes(run.progress.bytesTotal)}. A large setup takes a while; keep Agora open.`
              : 'Starting the import. Keep Agora open.'}
          </p>
        )}

        {run.status === 'error' && (
          <p role="alert" className="rounded-xl border border-destructive/40 bg-destructive/10 p-4 text-sm">
            The import did not finish: {run.message} Preview the import again to see what is still to do.
          </p>
        )}
      </section>
    );
  }

  if (page === 'profile' && chosen) {
    return (
      <section ref={pageTop} className="scroll-mt-4 space-y-5" data-testid="import-page">
        <div className="flex flex-wrap items-center gap-3">
          <BackButton label="Setups" onBack={() => setPage('setup')} />
          <p className="min-w-0 flex-1 text-sm">
            <span className="font-medium">{chosen.gameName ?? 'Game not named'}</span>
            <span className="block break-all font-mono text-xs text-muted-foreground">{chosen.iniPath}</span>
          </p>
        </div>

        <div className={`${STEP_CARD} space-y-4`} data-testid="import-form">
          <h3 className="text-base font-semibold">Choose a profile</h3>
          {chosen.profiles.length === 0 ? (
            <p className="text-sm text-muted-foreground">This setup has no profiles, so there is nothing to import.</p>
          ) : (
            <>
              <label className="grid gap-1 text-sm">
                <span className="font-medium">Profile</span>
                <select
                  className="rounded-lg border border-border bg-background px-3 py-2"
                  value={profile}
                  onChange={(event) => {
                    setProfile(event.target.value);
                    resetPreview();
                  }}
                >
                  {chosen.profiles.map((name) => (
                    <option key={name} value={name}>
                      {name}
                    </option>
                  ))}
                </select>
              </label>

              <label className="grid gap-1 text-sm">
                <span className="font-medium">Instance name (optional)</span>
                <span className="text-muted-foreground">
                  Leave this empty to name it after the profile. Only needed to import the same profile a second time.
                </span>
                <input
                  className="rounded-lg border border-border bg-background px-3 py-2"
                  value={instanceName}
                  onChange={(event) => {
                    setInstanceName(event.target.value);
                    resetPreview();
                  }}
                  placeholder={profile ? `${profile} (MO2 import)` : ''}
                />
              </label>

              <div className="flex items-start justify-between gap-3 text-sm">
                <span>
                  <span className="font-medium">Copy this profile’s saves into the new instance</span>
                  <span className="block text-muted-foreground">
                    Saves are copied into the instance’s own save folder under your Documents. Files that already exist
                    there are left alone. Off: the saves stay where they are, and you can copy them by hand.
                  </span>
                </span>
                <Switch
                  aria-label="Copy this profile’s saves into the new instance"
                  checked={copySaves}
                  onCheckedChange={(next) => {
                    setCopySaves(next);
                    resetPreview();
                  }}
                />
              </div>

              <div>
                <Button
                  onClick={() => void previewImport()}
                  disabled={!profile || plan.status === 'busy' || run.status === 'running'}
                >
                  {plan.status === 'busy' ? 'Reading the profile…' : 'Preview the import'}
                  {plan.status !== 'busy' && <ArrowRight aria-hidden="true" />}
                </Button>
              </div>

              {plan.status === 'error' && <PreviewError message={plan.message} noInstall={plan.noInstall} />}
            </>
          )}
        </div>
      </section>
    );
  }

  return (
    <section ref={pageTop} className="scroll-mt-4 space-y-5" data-testid="import-page">
      <div className={STEP_CARD}>
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h3 className="text-base font-semibold">Setups on this computer</h3>
          <div className="flex gap-2">
            <Button variant="outline" size="sm" onClick={loadSetups} disabled={scan.status === 'loading'}>
              <RefreshCw className="mr-2 h-4 w-4" aria-hidden="true" />
              Look again
            </Button>
            <Button variant="outline" size="sm" onClick={pickFolder}>
              <FolderOpen className="mr-2 h-4 w-4" aria-hidden="true" />
              Choose a setup folder…
            </Button>
          </div>
        </div>

        {scan.status === 'loading' && (
          <p role="status" className="text-sm text-muted-foreground">
            Looking for Mod Organizer 2 setups. Agora checks the usual place and your drives, which can take a minute.
          </p>
        )}

        {scan.status === 'error' && (
          <p role="alert" className="text-sm text-destructive">
            Agora could not look for setups: {scan.message}
          </p>
        )}

        {scan.status === 'loaded' && scan.setups.length === 0 && (
          <p className="text-sm text-muted-foreground" data-testid="import-empty">
            No Mod Organizer 2 setups were found in the usual place or on your drives. If yours is somewhere else, choose
            the folder it is in.
          </p>
        )}

        {scan.status === 'loaded' && scan.setups.length > 0 && (
          <ul className="space-y-2" aria-label="Setups found">
            {scan.setups.map((setup) => {
              const isChosen = chosen?.iniPath === setup.iniPath;
              return (
                <li
                  key={setup.iniPath}
                  className={[
                    'flex flex-wrap items-center justify-between gap-3 rounded-lg border p-3 text-sm',
                    isChosen ? 'border-primary/50 bg-primary/5' : 'border-border',
                  ].join(' ')}
                >
                  <span className="min-w-0 flex-1">
                    <span className="font-medium">{setup.gameName ?? 'Game not named'}</span>
                    <span className="block break-all font-mono text-xs text-muted-foreground">{setup.iniPath}</span>
                    <span className="block text-muted-foreground">{setupSummary(setup)}</span>
                  </span>
                  <Button size="sm" onClick={() => choose(setup)}>
                    Use this setup
                    <ArrowRight aria-hidden="true" />
                  </Button>
                </li>
              );
            })}
          </ul>
        )}

        {scan.status === 'loaded' &&
          scan.warnings.map((warning) => (
            <p key={warning} className="text-sm text-amber-700 dark:text-amber-300">
              {warning}
            </p>
          ))}

        {pickError && (
          <p role="alert" className="text-sm text-destructive">
            {pickError}
          </p>
        )}
      </div>
    </section>
  );
}

/** A preview that failed. When the game has no install Agora found, offer the Installs page in Settings. */
function PreviewError({ message, noInstall }: { message: string; noInstall: boolean }) {
  // It appears under the form's button, often below the fold of the dialog: bring it into view.
  const box = useRef<HTMLDivElement | null>(null);
  useEffect(() => {
    box.current?.scrollIntoView?.({ block: 'nearest' });
  }, [message]);
  return (
    <div ref={box} role="alert" className="space-y-2 rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-sm">
      <p>The preview failed. {message}</p>
      {noInstall && (
        <Button type="button" variant="outline" size="sm" onClick={() => openGameSettings('installs')}>
          Open Installs in Settings
          <ArrowRight aria-hidden="true" />
        </Button>
      )}
    </div>
  );
}

function RunSummary({ report }: { report: Mo2RunDto }) {
  return (
    <section
      aria-labelledby="import-done-heading"
      className="space-y-3 rounded-xl border border-emerald-500/40 bg-emerald-500/5 p-5"
      data-testid="import-done"
    >
      <h3 id="import-done-heading" className="flex items-center gap-2 text-base font-semibold">
        <CheckCircle2 className="h-4 w-4 text-emerald-600 dark:text-emerald-400" aria-hidden="true" />
        Imported as “{report.instanceName}”
      </h3>
      <p className="text-sm">
        {plural(report.storedMods, 'mod')} stored; {plural(report.contentLayers, 'content layer')}, {report.disabledLayers} of them
        disabled.
        {report.baseId && (
          <>
            {' '}
            Base {report.baseId} {report.baseBuilt ? 'was built' : 'was reused'}.
          </>
        )}
      </p>
      {report.duplicates.length > 0 && (
        <p className="text-sm text-muted-foreground">Already in this instance (same bytes): {report.duplicates.join(', ')}</p>
      )}
      {report.generatedLayers.length > 0 && (
        <p className="text-sm text-muted-foreground">
          Generated layers (inputs unknown): {report.generatedLayers.join(', ')}
        </p>
      )}
      {report.pluginFindings.length > 0 && (
        <ul className="space-y-1 text-sm" aria-label="Plugin order findings">
          {report.pluginFindings.map((finding) => (
            <li key={finding}>{finding}</li>
          ))}
        </ul>
      )}
      {report.nextSteps.length > 0 && (
        <div>
          <h4 className="text-sm font-medium">Still to do</h4>
          <ul className="mt-1 space-y-1 text-sm">
            {report.nextSteps.map((step) => (
              <li key={step}>{step}</li>
            ))}
          </ul>
        </div>
      )}
    </section>
  );
}
