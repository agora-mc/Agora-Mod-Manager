import { useCallback, useEffect, useState } from 'react';
import { Gamepad2 } from 'lucide-react';
import { formatError } from '../../lib/tauri';
import { MoreGamesCard } from '../../features/games/support/MoreGames';
import { listGameSupport, setGameEnabled, type GameSupportRow } from '../../features/games/support/api';
import { listOfficialPlugins } from '../../features/plugins/api';
import { OfficialBadge, OfficialStatusNote } from '../../features/plugins/OfficialBadge';
import type { OfficialPluginInfo } from '../../features/plugins/types';
import { SettingsSection } from './SettingsSection';

type Load =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; rows: GameSupportRow[] };

function sourceLabel(row: GameSupportRow): string {
  return row.source.kind === 'builtIn' ? 'Built in' : `From plugin ${row.source.pluginId}`;
}

/**
 * Game support: every game Agora knows, each with a box to offer it or not. A game that is turned
 * off is hidden from the game switcher. Its instances, bases and files are not touched.
 */
export function GameSupportSettings() {
  const [load, setLoad] = useState<Load>({ status: 'loading' });
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

  useEffect(() => {
    reload();
  }, [reload]);

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
                </div>
                <input
                  type="checkbox"
                  aria-label={`Offer ${row.name}`}
                  checked={row.enabled}
                  disabled={!row.canDisable || saving !== null}
                  onChange={(event) => void toggle(row, event.currentTarget.checked)}
                  className="h-5 w-5 shrink-0 accent-primary disabled:opacity-50"
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
