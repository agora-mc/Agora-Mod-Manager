import { useEffect, useState } from 'react';
import { Gamepad2 } from 'lucide-react';
import { formatError } from '../../lib/tauri';
import { listInstalledGames, type GameListing } from '../../features/games/discovery/api';
import { listBases, type BaseSummary } from '../../features/games/bases/api';
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

/**
 * The selected game's own settings, read-only for now: where Agora found it,
 * the bases it keeps, and the way to the game-support toggles. It never scans
 * the disk itself; both lists come from the commands the rest of the games UI
 * already uses.
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

  useEffect(() => {
    let cancelled = false;
    setGames({ status: 'loading' });
    setBases({ status: 'loading' });

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

    return () => {
      cancelled = true;
    };
  }, [gameId]);

  const listing = games.status === 'loaded' ? games.value : null;
  const name = gameDisplayName(gameId, listing?.name ?? knownName);
  const installs = listing?.installs ?? [];

  return (
    <SettingsSection
      card="selected-game"
      icon={Gamepad2}
      title={name}
      description="Read-only for now. Minecraft's own settings appear when Minecraft is selected."
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
