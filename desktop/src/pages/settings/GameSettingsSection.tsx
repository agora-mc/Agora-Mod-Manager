import { useEffect, useId, useState } from 'react';
import { Gamepad2 } from 'lucide-react';
import { formatError } from '../../lib/tauri';
import { listInstalledGames, type GameListing } from '../../features/games/discovery/api';
import { listBases, type BaseSummary } from '../../features/games/bases/api';
import {
  loadGameSettings,
  saveGameSetting,
  type GameSettings,
} from '../../features/games/settings/gameSettings';
import { SettingsSection } from './SettingsSection';
import { gameDisplayName } from './gameScope';

type Load<T> =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; value: T };

function formatBytes(bytes: number): string {
  if (bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${value >= 10 || exponent === 0 ? Math.round(value) : value.toFixed(1)} ${units[exponent]}`;
}

interface Choice<Value extends string> {
  value: Value;
  label: string;
}

const BASE_TYPE_OPTIONS: readonly Choice<GameSettings['baseType']>[] = [
  { value: 'linked', label: 'Linked (default)' },
  { value: 'copied', label: 'Copied' },
];

const DEPLOYMENT_OPTIONS: readonly Choice<GameSettings['deployment']>[] = [
  { value: 'auto', label: 'Automatic (default)' },
  { value: 'virtual', label: 'Virtual file system' },
  { value: 'links', label: 'Linked files' },
  { value: 'copies', label: 'Copied files' },
];

const IMPORT_SAVES_OPTIONS: readonly Choice<GameSettings['importCopySaves']>[] = [
  { value: 'off', label: 'Leave them where they are (default)' },
  { value: 'on', label: 'Copy them into the new instance' },
];

/**
 * One per-game choice. The visible label and the sentence under it are the setting's whole
 * explanation; the default is named in the option itself.
 */
function GameSettingSelect<Value extends string>({
  label,
  description,
  value,
  options,
  disabled,
  onChange,
}: {
  label: string;
  description: string;
  value: Value;
  options: readonly Choice<Value>[];
  disabled: boolean;
  onChange: (value: Value) => void;
}) {
  const id = useId();
  const descriptionId = useId();
  return (
    <label htmlFor={id} className="block space-y-1">
      <span className="block text-sm">{label}</span>
      <span id={descriptionId} className="block text-xs text-muted-foreground">
        {description}
      </span>
      <select
        id={id}
        aria-label={label}
        aria-describedby={descriptionId}
        className="flex h-9 w-full max-w-sm rounded-md border border-input bg-background px-3 py-1.5 text-sm"
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value as Value)}
      >
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
    </label>
  );
}

/**
 * The selected game's own section: where Agora found it, the bases it keeps, the defaults for
 * the next instance or base, and the way to the game-support toggles. It never scans the disk
 * itself; every list comes from the commands the rest of the games UI already uses.
 */
export function GameSettingsSection({
  gameId,
  gameName: knownName,
  onOpenGameSupport,
}: {
  gameId: string;
  /** A name the shell already knows; the discovery listing's name wins when there is one. */
  gameName?: string | null;
  onOpenGameSupport: () => void;
}) {
  const [games, setGames] = useState<Load<GameListing | null>>({ status: 'loading' });
  const [bases, setBases] = useState<Load<BaseSummary[]>>({ status: 'loading' });
  const [settings, setSettings] = useState<Load<GameSettings>>({ status: 'loading' });
  const [saving, setSaving] = useState<keyof GameSettings | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setGames({ status: 'loading' });
    setBases({ status: 'loading' });
    setSettings({ status: 'loading' });
    setSaveError(null);

    listInstalledGames()
      .then((list) => {
        if (cancelled) return;
        setGames({ status: 'loaded', value: list.games.find((game) => game.gameId === gameId) ?? null });
      })
      .catch((error: unknown) => {
        if (!cancelled) setGames({ status: 'failed', message: formatError(error) });
      });

    listBases(gameId)
      .then((reply) => {
        if (!cancelled) setBases({ status: 'loaded', value: reply.bases });
      })
      .catch((error: unknown) => {
        if (!cancelled) setBases({ status: 'failed', message: formatError(error) });
      });

    loadGameSettings(gameId)
      .then((value) => {
        if (!cancelled) setSettings({ status: 'loaded', value });
      })
      .catch((error: unknown) => {
        if (!cancelled) setSettings({ status: 'failed', message: formatError(error) });
      });

    return () => {
      cancelled = true;
    };
  }, [gameId]);

  const change = async <K extends keyof GameSettings>(name: K, value: GameSettings[K]) => {
    if (settings.status !== 'loaded') return;
    const previous = settings.value;
    setSettings({ status: 'loaded', value: { ...previous, [name]: value } });
    setSaving(name);
    setSaveError(null);
    try {
      await saveGameSetting(gameId, name, value);
    } catch (error) {
      setSettings({ status: 'loaded', value: previous });
      setSaveError(`That setting was not saved. ${formatError(error)}`);
    } finally {
      setSaving(null);
    }
  };

  const listing = games.status === 'loaded' ? games.value : null;
  const name = gameDisplayName(gameId, listing?.name ?? knownName);
  const installs = listing?.installs ?? [];
  const choices = settings.status === 'loaded' ? settings.value : null;

  return (
    <SettingsSection
      card="selected-game"
      icon={Gamepad2}
      title={name}
      description="Your choices for this game only. Minecraft's own settings appear when Minecraft is selected."
    >
      <div className="space-y-1">
        <p className="text-sm font-medium">Install folders</p>
        {games.status === 'loading' && <p className="text-xs text-muted-foreground">Looking for installs…</p>}
        {games.status === 'failed' && (
          <p role="alert" className="text-xs text-destructive">
            Agora could not look for installs of {name}. {games.message}
          </p>
        )}
        {games.status === 'loaded' && installs.length === 0 && (
          <p className="text-xs text-muted-foreground">
            Agora has not found an install of {name} on this computer.
          </p>
        )}
        {installs.length > 0 && (
          <ul className="space-y-0.5">
            {installs.map((install) => (
              <li key={install.installId} className="break-all text-xs text-muted-foreground">
                <span className="font-medium text-foreground">{install.location}</span>
                {install.store ? ` · ${install.store}` : ''}
              </li>
            ))}
          </ul>
        )}
      </div>

      <div className="space-y-1">
        <p className="text-sm font-medium">Bases</p>
        {bases.status === 'loading' && <p className="text-xs text-muted-foreground">Checking bases…</p>}
        {bases.status === 'failed' && (
          <p role="alert" className="text-xs text-destructive">
            Agora could not read the bases for {name}. {bases.message}
          </p>
        )}
        {bases.status === 'loaded' && bases.value.length === 0 && (
          <p className="text-xs text-muted-foreground">No bases built yet.</p>
        )}
        {bases.status === 'loaded' && bases.value.length > 0 && (
          <>
            <p className="text-xs text-muted-foreground">
              {bases.value.length} base{bases.value.length === 1 ? '' : 's'} for {name}, using{' '}
              {formatBytes(bases.value.reduce((sum, base) => sum + base.copiedBytes, 0))}.
            </p>
            <ul className="space-y-0.5">
              {bases.value.map((base) => (
                <li key={base.baseId} className="break-all text-xs text-muted-foreground">
                  {base.location}
                  {!base.present && ' · folder is missing'}
                </li>
              ))}
            </ul>
          </>
        )}
      </div>

      <div className="space-y-3">
        <div>
          <p className="text-sm font-medium">New instances and bases</p>
          <p className="text-xs text-muted-foreground">
            The choices here fill in the forms for {name}; you can still change them there.
          </p>
        </div>
        {settings.status === 'loading' && <p className="text-xs text-muted-foreground">Reading these settings…</p>}
        {settings.status === 'failed' && (
          <p role="alert" className="text-xs text-destructive">
            Agora could not read the settings for {name}. {settings.message}
          </p>
        )}
        {choices !== null && (
          <>
            <GameSettingSelect
              label="Base type for new instances and bases"
              description="Linked shares most game files with the install and takes little space. Copied keeps its own copy of the game files."
              value={choices.baseType}
              options={BASE_TYPE_OPTIONS}
              disabled={saving !== null}
              onChange={(value) => void change('baseType', value)}
            />
            <GameSettingSelect
              label="How new instances run"
              description="Automatic uses the game's own choice. Another mode is set on each new instance and can be changed on its page."
              value={choices.deployment}
              options={DEPLOYMENT_OPTIONS}
              disabled={saving !== null}
              onChange={(value) => void change('deployment', value)}
            />
            <GameSettingSelect
              label="Saves when importing a profile"
              description="An imported profile can keep its saves where they are, or copy them into the new instance's own save folder."
              value={choices.importCopySaves}
              options={IMPORT_SAVES_OPTIONS}
              disabled={saving !== null}
              onChange={(value) => void change('importCopySaves', value)}
            />
          </>
        )}
        {saveError !== null && (
          <p role="alert" className="text-xs text-destructive">{saveError}</p>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-2 pt-1">
        <button
          type="button"
          onClick={onOpenGameSupport}
          className="rounded-lg border border-input px-3 py-1.5 text-xs font-medium hover:bg-accent"
        >
          Game support toggles
        </button>
        <p className="text-xs text-muted-foreground">Turn games on or off, or choose another game's settings.</p>
      </div>
    </SettingsSection>
  );
}
