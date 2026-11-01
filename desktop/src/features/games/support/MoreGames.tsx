import { useCallback, useEffect, useState } from 'react';
import { Gamepad2, X } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { formatError, getSetting, parseLauncherError, restartApp, setSetting } from '@/lib/tauri';
import {
  installOfficialPlugin,
  listOfficialPlugins,
} from '@/features/plugins/api';
import { InstallPrompt } from '@/features/plugins/InstallPrompt';
import type { InstallPreview, OfficialPluginInfo } from '@/features/plugins/types';

/**
 * The "More games" card: official plugins that add a game Agora does not have
 * yet. It is the way a player notices that Agora is not Minecraft-only, and the
 * way they add the game without opening the plugin manager.
 *
 * An official game package is data only, so it loads without plugin code consent
 * and regardless of the emergency stop; this card never asks about either. The
 * consent prompt is the same one every other install path uses, and it asks the
 * per-plugin code question only when the package actually runs code.
 */

/** The copy for the card, in one place so the wording can change without touching logic. */
export const MORE_GAMES_COPY = {
  title: 'More games',
  description: 'Agora can run other games too. Install one to add it to your game switcher.',
  install: 'Install',
  installing: 'Installing…',
  dismiss: 'Dismiss',
  empty: 'Nothing else is available to install right now.',
  unavailable: 'Agora could not load the list of more games.',
  hashMismatch: 'The download did not match what Agora expected, so nothing was installed.',
  restart: (name: string) => `Restart Agora to add ${name}.`,
  restartNow: 'Restart now',
} as const;

/** Remembered on Home after the player dismisses the card. */
export const MORE_GAMES_DISMISSED_SETTING = 'more_games_dismissed';

type Load =
  | { status: 'loading' }
  | { status: 'failed' }
  | { status: 'loaded'; plugins: OfficialPluginInfo[] };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isOfficialInfo(value: unknown): value is OfficialPluginInfo {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.name === 'string' &&
    Array.isArray(value.gameIds) &&
    value.gameIds.every((gameId) => typeof gameId === 'string') &&
    typeof value.version === 'string' &&
    typeof value.state === 'string'
  );
}

/** A game plugin not on this machine yet. A catalog problem is still shown, with the reason. */
function offerable(info: OfficialPluginInfo): boolean {
  return info.state === 'notInstalled' && info.gameIds.length > 0;
}

/**
 * A single message for an install failure. The hash case is spelled out for a
 * player: what happened, and that nothing changed on their machine.
 */
function installFailure(error: unknown): string {
  const parsed = parseLauncherError(error);
  if (parsed.code === 'ERR_OFFICIAL_PLUGIN_HASH') return MORE_GAMES_COPY.hashMismatch;
  return formatError(error);
}

export function MoreGamesCard({ surface }: { surface: 'home' | 'settings' }) {
  const [load, setLoad] = useState<Load>({ status: 'loading' });
  const [dismissed, setDismissed] = useState(false);
  const [dismissedRead, setDismissedRead] = useState(false);
  const [installingId, setInstallingId] = useState<string | null>(null);
  const [consent, setConsent] = useState<{ info: OfficialPluginInfo; preview: InstallPreview } | null>(null);
  const [added, setAdded] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    setLoad({ status: 'loading' });
    listOfficialPlugins()
      .then((reply) => {
        if (!isRecord(reply) || !Array.isArray(reply.plugins)) {
          setLoad({ status: 'failed' });
          return;
        }
        if (typeof reply.unavailableReason === 'string') {
          setLoad({ status: 'failed' });
          return;
        }
        setLoad({
          status: 'loaded',
          plugins: reply.plugins.filter(isOfficialInfo).filter(offerable),
        });
      })
      .catch(() => setLoad({ status: 'failed' }));
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  useEffect(() => {
    if (surface !== 'home') {
      setDismissedRead(true);
      return;
    }
    getSetting(MORE_GAMES_DISMISSED_SETTING)
      .then((value) => setDismissed(value === true))
      .catch(() => setDismissed(false))
      .finally(() => setDismissedRead(true));
  }, [surface]);

  const dismiss = () => {
    setDismissed(true);
    void setSetting(MORE_GAMES_DISMISSED_SETTING, true).catch(() => {
      // The card is still hidden for this session; the next start may show it again.
    });
  };

  const installed = useCallback((info: OfficialPluginInfo) => {
    setConsent(null);
    setInstallingId(null);
    setError(null);
    setAdded(info.name);
    setLoad((current) =>
      current.status === 'loaded'
        ? { ...current, plugins: current.plugins.filter((entry) => entry.id !== info.id) }
        : current,
    );
  }, []);

  const startInstall = useCallback(async (info: OfficialPluginInfo) => {
    setError(null);
    setInstallingId(info.id);
    try {
      const result = await installOfficialPlugin(info.id, false);
      if (isRecord(result) && result.outcome === 'needsConsent' && isRecord(result.preview)) {
        setConsent({ info, preview: result.preview as unknown as InstallPreview });
        return;
      }
      if (isRecord(result) && result.outcome === 'installed') {
        installed(info);
        return;
      }
      setError('Agora did not return a usable install result.');
    } catch (failure) {
      setError(installFailure(failure));
    } finally {
      setInstallingId(null);
    }
  }, [installed]);

  const confirmInstall = useCallback(async (allowCode: boolean) => {
    if (!consent) return;
    setError(null);
    setInstallingId(consent.info.id);
    try {
      const result = await installOfficialPlugin(consent.info.id, true, allowCode);
      if (isRecord(result) && result.outcome === 'needsConsent') {
        setError('Agora asked for the same permission again, so nothing was installed.');
        return;
      }
      if (isRecord(result) && result.outcome === 'installed') {
        installed(consent.info);
        return;
      }
      setError('Agora did not return a usable install result.');
    } catch (failure) {
      setError(installFailure(failure));
    } finally {
      setInstallingId(null);
    }
  }, [consent, installed]);

  // Home stays quiet while loading, after a failed read, and when there is
  // nothing new. Settings always shows the card and says what happened.
  if (surface === 'home') {
    if (!dismissedRead || dismissed) return null;
    if (load.status === 'failed') return null;
    if (load.status === 'loaded' && load.plugins.length === 0 && added === null) return null;
  }

  return (
    <section
      className="rounded-xl border border-border bg-card p-6"
      aria-labelledby="more-games-title"
      data-testid="more-games"
    >
      <div className="flex items-start justify-between gap-3">
        <div className="flex items-center gap-2">
          <Gamepad2 className="h-5 w-5 text-primary" aria-hidden="true" />
          <h3 id="more-games-title" className="text-lg font-semibold">
            {MORE_GAMES_COPY.title}
          </h3>
        </div>
        {surface === 'home' ? (
          <Button variant="ghost" size="sm" onClick={dismiss} aria-label={MORE_GAMES_COPY.dismiss}>
            <X className="h-4 w-4" />
          </Button>
        ) : null}
      </div>
      <p className="mt-1 text-sm text-muted-foreground">{MORE_GAMES_COPY.description}</p>

      {added ? (
        <div className="mt-3 rounded-lg border border-border bg-muted/30 p-3" role="status">
          <p className="text-sm">{MORE_GAMES_COPY.restart(added)}</p>
          <Button size="sm" className="mt-2" onClick={() => void restartApp()}>
            {MORE_GAMES_COPY.restartNow}
          </Button>
        </div>
      ) : null}

      {load.status === 'loading' ? (
        <p className="mt-3 text-xs text-muted-foreground">Loading…</p>
      ) : null}

      {load.status === 'failed' && surface === 'settings' ? (
        <p role="alert" className="mt-3 text-xs text-destructive">
          {MORE_GAMES_COPY.unavailable}
        </p>
      ) : null}

      {load.status === 'loaded' && load.plugins.length === 0 && added === null ? (
        <p className="mt-3 text-xs text-muted-foreground">{MORE_GAMES_COPY.empty}</p>
      ) : null}

      {load.status === 'loaded' && load.plugins.length > 0 ? (
        <ul className="mt-3 divide-y divide-border rounded-lg border border-border">
          {load.plugins.map((info) => (
            <li key={info.id} className="flex items-center justify-between gap-3 px-3 py-2">
              <div className="min-w-0">
                <p className="truncate text-sm">{info.name}</p>
                <p className="text-xs text-muted-foreground">
                  {info.description ?? `Adds ${info.gameIds.join(', ')} to Agora.`}
                </p>
                {info.problem ? (
                  <p className="text-xs text-amber-600 dark:text-amber-400">{info.problem}</p>
                ) : null}
              </div>
              <Button
                size="sm"
                disabled={installingId !== null || consent !== null || Boolean(info.problem)}
                onClick={() => void startInstall(info)}
                aria-label={`${MORE_GAMES_COPY.install} ${info.name}`}
              >
                {installingId === info.id ? MORE_GAMES_COPY.installing : MORE_GAMES_COPY.install}
              </Button>
            </li>
          ))}
        </ul>
      ) : null}

      {consent ? (
        <div className="mt-3">
          <InstallPrompt
            preview={consent.preview}
            development={false}
            busy={installingId !== null}
            onCancel={() => setConsent(null)}
            onConfirm={(allowCode) => void confirmInstall(allowCode)}
          />
        </div>
      ) : null}

      {error ? (
        <p role="alert" className="mt-3 text-xs text-destructive">
          {error}
        </p>
      ) : null}
    </section>
  );
}
