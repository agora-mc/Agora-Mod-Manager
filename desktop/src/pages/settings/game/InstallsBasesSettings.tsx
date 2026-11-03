import { useEffect, useState } from 'react';
import { ArrowRight, FolderOpen, Layers } from 'lucide-react';
import { formatError } from '../../../lib/tauri';
import { listInstalledGames, type GameListing } from '../../../features/games/discovery/api';
import { listBases, type BaseSummary } from '../../../features/games/bases/api';
import { formatBytes } from '../../../features/games/content/api';
import { storeName } from '../../../features/games/shared/stores';
import type { InstallSummary } from '../../../features/games/shared/types';
import { SettingsSection } from '../SettingsSection';

type Load<T> =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; value: T };

const OUTLINE_BUTTON = 'inline-flex shrink-0 items-center gap-1.5 rounded-lg border border-input px-3 py-1.5 text-xs font-medium hover:bg-accent';

function installVersion(install: InstallSummary): string | null {
  if (install.runtime.status === 'identified') return install.runtime.runtime.version;
  return install.storeVersion;
}

/**
 * The selected game's "Installs & bases" page: where Agora found the game, and the bases it keeps
 * for it. Read-only here; Setup is where installs are added and bases are built, checked and
 * removed, so each card leads there. It never scans the disk itself: both lists come from the
 * commands the rest of the games UI already uses.
 */
export function InstallsBasesSettings({
  gameId,
  gameName,
  onOpenSetup,
  onOpenGameSupport,
}: {
  gameId: string;
  gameName: string;
  /** Opens this game's Setup page; absent where Settings is shown without the shell. */
  onOpenSetup?: () => void;
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
        if (!cancelled) setGames({ status: 'loaded', value: list.games.find((game) => game.gameId === gameId) ?? null });
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

  const installs = games.status === 'loaded' ? games.value?.installs ?? [] : [];
  const setupButton = (label: string) => onOpenSetup && (
    <button type="button" onClick={onOpenSetup} className={OUTLINE_BUTTON}>
      {label}
      <ArrowRight className="h-3.5 w-3.5" aria-hidden="true" />
    </button>
  );

  return (
    <>
      <SettingsSection
        card="game-installs"
        icon={FolderOpen}
        title="Install folders"
        description={`Where Agora found ${gameName} on this computer. A copy it did not find can be added in Setup.`}
        action={setupButton('Open Setup')}
      >
        {games.status === 'loading' && <p className="text-xs text-muted-foreground">Looking for installs…</p>}
        {games.status === 'failed' && (
          <p role="alert" className="text-xs text-destructive">
            Agora could not look for installs of {gameName}. {games.message}
          </p>
        )}
        {games.status === 'loaded' && installs.length === 0 && (
          <p className="text-xs text-muted-foreground">
            Agora has not found an install of {gameName} on this computer.
          </p>
        )}
        {installs.length > 0 && (
          <ul className="divide-y divide-border rounded-lg border border-border">
            {installs.map((install) => {
              const version = installVersion(install);
              return (
                <li key={install.installId} className="px-3 py-2">
                  <p className="text-sm font-medium">
                    {storeName(install.store)}
                    {version && <span className="font-normal text-muted-foreground"> · {version}</span>}
                  </p>
                  <p className="break-all font-mono text-xs text-muted-foreground">{install.location}</p>
                </li>
              );
            })}
          </ul>
        )}
        <p className="text-xs text-muted-foreground">
          To stop offering {gameName}, turn it off in{' '}
          <button type="button" onClick={onOpenGameSupport} className="font-medium text-primary hover:underline">
            Game support
          </button>
          .
        </p>
      </SettingsSection>

      <SettingsSection
        card="game-bases"
        icon={Layers}
        title="Bases"
        description="A base is a verified copy of one version of the game. Instances built from it keep working when your store updates the game."
        action={setupButton('Manage in Setup')}
      >
        {bases.status === 'loading' && <p className="text-xs text-muted-foreground">Checking bases…</p>}
        {bases.status === 'failed' && (
          <p role="alert" className="text-xs text-destructive">
            Agora could not read the bases for {gameName}. {bases.message}
          </p>
        )}
        {bases.status === 'loaded' && bases.value.length === 0 && (
          <p className="text-xs text-muted-foreground">No bases built yet.</p>
        )}
        {bases.status === 'loaded' && bases.value.length > 0 && (
          <>
            <p className="text-sm">
              {bases.value.length} base{bases.value.length === 1 ? '' : 's'} for {gameName}, using{' '}
              <span className="font-medium">{formatBytes(bases.value.reduce((sum, base) => sum + base.copiedBytes, 0))}</span>{' '}
              of their own.
            </p>
            <ul className="divide-y divide-border rounded-lg border border-border">
              {bases.value.map((base) => (
                <li key={base.baseId} className="px-3 py-2">
                  <p className="text-sm font-medium">
                    {storeName(base.store)} {base.version}
                    <span className="font-normal text-muted-foreground">
                      {' · '}{base.mode === 'linked' ? 'Linked' : 'Copied'}
                      {base.usedBy.length > 0 && ` · used by ${base.usedBy.length} instance${base.usedBy.length === 1 ? '' : 's'}`}
                    </span>
                    {!base.present && <span className="font-normal text-destructive"> · folder is missing</span>}
                  </p>
                  <p className="break-all font-mono text-xs text-muted-foreground">{base.location}</p>
                </li>
              ))}
            </ul>
          </>
        )}
      </SettingsSection>
    </>
  );
}
