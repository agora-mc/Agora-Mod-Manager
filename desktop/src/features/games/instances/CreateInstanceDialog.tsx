import { useEffect, useState } from 'react';
import {
  ChoiceCards,
  CreateInstanceFrame,
  FORM_CONTROL_CLASS,
  FormField,
  recommendedLabel,
  type ChoiceOption,
} from '@/components/instance-kit';
import { openGameSettings } from '@/lib/settingsSection';
import { listInstalledGames } from '../discovery/api';
import type { GameListing, InstallSummary } from '../shared/types';
import { listGameSupport, type GameSupportRow } from '../support/api';
import { isGameTurnedOff } from '../support/enabled';
import { loadGameSettingsOrDefaults, RECOMMENDED_GAME_SETTINGS, type GameSettings } from '../settings/gameSettings';
import {
  createGameInstance,
  gamesErrorMessage,
  onCreateProgress,
  type BaseMode,
  type CreateProgress,
} from './api';
import { baseModeCopy, DEPLOYMENT_OPTIONS, progressText } from './instanceText';
import { runtimeText } from './instanceIdentity';

interface CreateInstanceDialogProps {
  open: boolean;
  gameId: string;
  onOpenChange: (open: boolean) => void;
  /** Called with the new instance's id once it exists. */
  onCreated: (instanceId: string) => void;
}

type InstallsState =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; game: GameListing | null; turnedOff: boolean };

/** The base choices, with the one Agora advises (from the same source as Settings) badged. */
const BASE_CHOICES: readonly ChoiceOption<BaseMode>[] = (['linked', 'copied'] as const).map((mode) => ({
  value: mode,
  label: baseModeCopy(mode).label,
  description: baseModeCopy(mode).explain,
  recommended: mode === RECOMMENDED_GAME_SETTINGS.baseType,
}));

/**
 * Creates an instance from an install of the game. The base is built from the install (or an
 * identical one is reused). The deployment choice is explained here and made on the instance's
 * own page, where it can be changed later.
 */
export function CreateInstanceDialog({ open, gameId, onOpenChange, onCreated }: CreateInstanceDialogProps) {
  const [installs, setInstalls] = useState<InstallsState>({ status: 'loading' });
  const [name, setName] = useState('');
  const [chosenInstall, setChosenInstall] = useState('');
  const [baseMode, setBaseMode] = useState<BaseMode>('linked');
  const [deployment, setDeployment] = useState<GameSettings['deployment']>('auto');
  const [creating, setCreating] = useState(false);
  const [progress, setProgress] = useState<CreateProgress | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setInstalls({ status: 'loading' });
    setError(null);
    Promise.all([
      listInstalledGames(),
      // A support list that could not be read hides nothing, the same as in the game switcher.
      listGameSupport().catch((): GameSupportRow[] | null => null),
      loadGameSettingsOrDefaults(gameId),
    ])
      .then(([list, support, defaults]) => {
        const turnedOff = isGameTurnedOff(gameId, support);
        const game = turnedOff
          ? null
          : list.games.find((listing) => listing.gameId === gameId) ?? null;
        setInstalls({ status: 'loaded', game, turnedOff });
        setChosenInstall(game?.installs[0]?.installId ?? '');
        setBaseMode(defaults.baseType);
        setDeployment(defaults.deployment);
      })
      .catch((failure: unknown) => setInstalls({ status: 'failed', message: gamesErrorMessage(failure) }));
  }, [open, gameId]);

  const available: InstallSummary[] = installs.status === 'loaded' ? (installs.game?.installs ?? []) : [];
  const gameName = installs.status === 'loaded' && installs.game ? installs.game.name : 'this game';
  const selected = available.find((install) => install.installId === chosenInstall) ?? null;
  const canCreate = !creating && name.trim() !== '' && selected !== null;

  const submit = async () => {
    if (!canCreate || selected === null) return;
    setCreating(true);
    setError(null);
    setProgress(null);
    const unlisten = await onCreateProgress(setProgress).catch(() => null);
    try {
      const created = await createGameInstance(gameId, selected.installId, name, baseMode, deployment);
      onCreated(created.instanceId);
    } catch (failure) {
      setError(`The instance was not created. ${gamesErrorMessage(failure)}`);
    } finally {
      unlisten?.();
      setCreating(false);
    }
  };

  const stateNote = (text: string, tone: 'status' | 'alert') => (
    <p role={tone} className={tone === 'status' ? 'text-sm text-muted-foreground' : 'rounded-lg border border-border bg-muted/40 p-3 text-sm'}>
      {text}
    </p>
  );
  const ready = installs.status === 'loaded' && !installs.turnedOff && available.length > 0;
  const deploymentOption = DEPLOYMENT_OPTIONS.find((option) => option.id === deployment);

  return (
    <CreateInstanceFrame
      open={open}
      onOpenChange={onOpenChange}
      testId="create-game-instance-dialog"
      title={`New instance of ${gameName}`}
      description="An instance is a separate setup of the game. Its mods and saves stay with it, and the game install is shared."
      onSubmit={() => void submit()}
      canSubmit={canCreate}
      busy={creating}
      progress={creating
        ? `Building the base… ${progress !== null ? progressText(progress.filesDone, progress.filesTotal) : 'Starting…'}`
        : null}
      error={error}
      footer={ready ? undefined : (
        <div className="flex justify-end pt-2">
          <button
            type="button"
            onClick={() => onOpenChange(false)}
            className="rounded-lg border border-input bg-background px-4 py-2 text-sm font-medium hover:bg-accent"
          >
            Close
          </button>
        </div>
      )}
    >
      {installs.status === 'loading' && stateNote('Looking for installs…', 'status')}
      {installs.status === 'failed' && (
        <p role="alert" className="text-sm text-destructive">Agora could not look for installs. {installs.message}</p>
      )}
      {installs.status === 'loaded' && installs.turnedOff && stateNote(
        'This game is turned off, so it is not offered here. Turn it on in Settings, Game support, to create an instance.',
        'alert',
      )}
      {installs.status === 'loaded' && !installs.turnedOff && available.length === 0 && (
        <p role="alert" className="rounded-lg border border-border bg-muted/40 p-3 text-sm">
          Agora has not found an install of {gameName} on this machine. Rescan or add its folder on the{' '}
          <button
            type="button"
            className="font-medium text-primary hover:underline"
            onClick={() => {
              onOpenChange(false);
              openGameSettings('installs');
            }}
          >
            Installs page in Settings
          </button>
          , then try again.
        </p>
      )}

      {ready && (
        <>
          <FormField label="Name" hint={name.trim() === '' ? 'Type a name to enable Create.' : undefined}>
            {(control) => (
              <input
                {...control}
                value={name}
                placeholder="e.g. Survival playthrough"
                onChange={(event) => setName(event.target.value)}
                disabled={creating}
                autoComplete="off"
                autoFocus
                className={FORM_CONTROL_CLASS}
              />
            )}
          </FormField>

          <FormField label="Install to build from" hint={selected?.location}>
            {(control) => (
              <select
                {...control}
                className={FORM_CONTROL_CLASS}
                value={chosenInstall}
                onChange={(event) => setChosenInstall(event.target.value)}
                disabled={creating}
              >
                {available.map((install) => (
                  <option key={install.installId} value={install.installId}>
                    {install.name} · {runtimeText(install.store, install.storeVersion) ?? install.store}
                  </option>
                ))}
              </select>
            )}
          </FormField>

          <div>
            <span className="text-sm font-medium">Base</span>
            <ChoiceCards
              label="Base"
              options={BASE_CHOICES}
              value={baseMode}
              onChange={setBaseMode}
              disabled={creating}
              className="mt-1 sm:grid-cols-1"
            />
            <span className="mt-1 block text-xs text-muted-foreground">
              The base is a pinned copy of the game files that instances share. Agora reuses one that already exists.
            </span>
          </div>

          <FormField
            label="How it will run"
            hint={<>{deploymentOption?.explain} You can change this later on the instance's Overview.</>}
          >
            {(control) => (
              <select
                {...control}
                className={FORM_CONTROL_CLASS}
                value={deployment}
                onChange={(event) => setDeployment(event.target.value as GameSettings['deployment'])}
                disabled={creating}
              >
                {DEPLOYMENT_OPTIONS.map((option) => (
                  <option key={option.id} value={option.id}>
                    {recommendedLabel({ label: option.label, recommended: option.id === RECOMMENDED_GAME_SETTINGS.deployment })}
                  </option>
                ))}
              </select>
            )}
          </FormField>
        </>
      )}
    </CreateInstanceFrame>
  );
}
