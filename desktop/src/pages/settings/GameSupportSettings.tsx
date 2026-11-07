import { useCallback, useEffect, useState } from 'react';
import { Gamepad2 } from 'lucide-react';
import { formatError } from '../../lib/tauri';
import { MoreGamesCard } from '../../features/games/support/MoreGames';
import { listGameSupport, setGameEnabled, type GameSupportRow } from '../../features/games/support/api';
import { AddGameFolderButton } from '../../features/games/discovery';
import { listInstalledGames } from '../../features/games/discovery/api';
import { MINECRAFT_GAME_ID } from '../../features/games/shared/types';
import { listOfficialPlugins } from '../../features/plugins/api';
import { OfficialBadge, OfficialStatusNote } from '../../features/plugins/OfficialBadge';
import type { OfficialPluginInfo } from '../../features/plugins/types';
import { SettingsSection } from './SettingsSection';
import { Switch } from '../../components/ui/switch';

type Load =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; rows: GameSupportRow[] };

type Installs =
  | { status: 'loading' }
  | { status: 'failed' }
  | { status: 'loaded'; found: Set<string> };

function sourceLabel(row: GameSupportRow): string {
  return row.source.kind === 'builtIn' ? 'Built in' : `From plugin ${row.source.pluginId}`;
}

/**
 * Game support: every game Agora knows, each with a box to offer it or not. A game that is turned
 * off is hidden from the game switcher. Its instances, bases and files are not touched.
 */
export function GameSupportSettings() {
  const [load, setLoad] = useState<Load>({ status: 'loading' });
  const [installs, setInstalls] = useState<Installs>({ status: 'loading' });
  const [notice, setNotice] = useState<string | null>(null);
  const [addError, setAddError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  // One change at a time: the saved list is read, changed and written back as a whole.
  const [saving, setSaving] = useState<string | null>(null);
  // Official catalog entries by plugin id, so a game added by a plugin can say
  // whether that plugin is currently verified official. A failed read only
  // means no badge; it never hides a game.
  const [officialById, setOfficialById] = useState<Record<string, OfficialPluginInfo>>({});

  const loadOfficial = useCallback(() => {
    listOfficialPlugins()
      .then((reply) => {
        if (!reply || !Array.isArray(reply.plugins)) {
          setOfficialById({});
          return;
        }
        const byId: Record<string, OfficialPluginInfo> = {};
        for (const info of reply.plugins) {
          if (info && typeof info.id === 'string') byId[info.id] = info;
        }
        setOfficialById(byId);
      })
      .catch(() => setOfficialById({}));
  }, []);

  useEffect(() => {
    loadOfficial();
  }, [loadOfficial]);

  const reload = useCallback(() => {
    setLoad({ status: 'loading' });
    listGameSupport()
      .then((rows) => setLoad({ status: 'loaded', rows }))
      .catch((error: unknown) => setLoad({ status: 'failed', message: formatError(error) }));
  }, []);

  // Which games discovery found. A failed read is shown as "could not check", never as "not found".
  const loadInstalls = useCallback(() => {
    setInstalls({ status: 'loading' });
    listInstalledGames()
      .then((list) => {
        const found = new Set(
          list.games.filter((game) => game.installs.length > 0).map((game) => game.gameId),
        );
        setInstalls({ status: 'loaded', found });
      })
      .catch(() => setInstalls({ status: 'failed' }));
  }, []);

  useEffect(() => {
    reload();
    loadInstalls();
  }, [reload, loadInstalls]);

  const folderAdded = useCallback(async (added: { path: string; gameName: string }) => {
    setNotice(`Added ${added.path} as ${added.gameName}.`);
    setAddError(null);
    reload();
    loadInstalls();
  }, [reload, loadInstalls]);

  const toggle = async (row: GameSupportRow, enabled: boolean) => {
    setSaveError(null);
    setSaving(row.gameId);
    try {
      await setGameEnabled(row.gameId, enabled);
      setLoad((current) => current.status === 'loaded'
        ? {
          status: 'loaded',
          rows: current.rows.map((other) => (other.gameId === row.gameId ? { ...other, enabled } : other)),
        }
        : current);
    } catch (error) {
      setSaveError(`${row.name} was not changed. ${formatError(error)}`);
    } finally {
      setSaving(null);
    }
  };

  return (
    <SettingsSection
      card="game-support"
      icon={Gamepad2}
      title="Game support"
      description="Turn a game off to hide it from the game switcher. Its instances, bases and files stay where they are."
    >
      <MoreGamesCard surface="settings" />

      {notice && (
        <p role="status" className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs text-foreground">
          {notice}
        </p>
      )}

      {addError && <p role="alert" className="text-xs text-destructive">{addError}</p>}

      {load.status === 'loading' && <p className="text-xs text-muted-foreground">Loading…</p>}
      {load.status === 'failed' && (
        <div className="space-y-2">
          <p role="alert" className="text-xs text-destructive">
            Agora could not load the list of games. {load.message}
          </p>
          <button type="button" onClick={reload} className="text-xs text-primary hover:underline">
            Try again
          </button>
        </div>
      )}
      {load.status === 'loaded' && (
        <ul className="divide-y divide-border rounded-lg border border-border">
          {load.rows.map((row) => {
            const official = row.source.kind === 'plugin' ? officialById[row.source.pluginId] : null;
            const status = official?.installedStatus ?? null;
            const found = installs.status === 'loaded' ? installs.found.has(row.gameId) : null;
            return (
              <li key={row.gameId} className="flex items-center justify-between gap-3 px-3 py-2">
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <p className="truncate text-sm">{row.name}</p>
                    <OfficialBadge status={status} />
                  </div>
                  <p className="text-xs text-muted-foreground">
                    {sourceLabel(row)}
                    {!row.canDisable && ' · Always on: Agora falls back to Minecraft, so it cannot be turned off yet.'}
                  </p>
                  <OfficialStatusNote status={status} />
                  {row.gameId !== MINECRAFT_GAME_ID && (
                    <div className="mt-2 flex flex-wrap items-center gap-2">
                      <span className="text-xs text-muted-foreground">
                        {installs.status === 'loading'
                          ? 'Checking where this game is installed…'
                          : installs.status === 'failed'
                            ? 'Agora could not check where this game is installed.'
                            : found
                              ? 'Found on this computer.'
                              : 'Not found on this computer.'}
                      </span>
                      <AddGameFolderButton
                        label="Add game folder"
                        ariaLabel={`Add game folder for ${row.name}`}
                        variant="outline"
                        size="sm"
                        onError={(message) => {
                          setNotice(null);
                          setAddError(message);
                        }}
                        onAdded={folderAdded}
                      />
                    </div>
                  )}
                </div>
                <Switch
                  aria-label={`Offer ${row.name}`}
                  size="sm"
                  checked={row.enabled}
                  disabled={!row.canDisable || saving !== null}
                  onCheckedChange={(checked) => void toggle(row, checked)}
                />
              </li>
            );
          })}
        </ul>
      )}
      {saveError && <p role="alert" className="text-xs text-destructive">{saveError}</p>}
    </SettingsSection>
  );
}
