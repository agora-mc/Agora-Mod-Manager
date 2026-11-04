/**
 * Home for a game other than Minecraft: Minecraft's Home (`pages/Home.tsx`) with this game's
 * instances and catalog. The same header, the same "continue playing" card (with the instance's
 * tile and chips, as on My Instances), the same row of recent things, the same Help & Guide and
 * community cards, and the same "from the catalog" strip that Minecraft fills with recommendations.
 *
 * What Minecraft's Home has and a game's does not, yet: the overview of what Agora manages (its
 * terms are Minecraft's), the "More games" card, crash alerts and the last setup that launched
 * fine (games have no crash reports or snapshots), health notices (a game's checks run on the
 * instance page), Quick Play into a world or server, and the catalog status banner.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ArrowRight, Boxes, Download, Play, Plus, Square } from 'lucide-react';
import { CommunityCards, GuideCard, HomeHero, timeSince } from '@/components/home/HomeCards';
import { InstanceCardBanner, PageHeroAction, RunningIndicator } from '@/components/instance-kit';
import { requestGameImport } from '@/lib/gameImportRequest';
import { listCatalogEntries, type CatalogEntry } from '../catalog/api';
import { listGameInstances, onInstanceExited, type InstanceCard } from '../instances/api';
import { parseStoredTime } from '../instances/instanceDates';
import { gameKitCapabilities, GameInstanceTile, InstanceChips } from '../instances/instanceIdentity';
import { playedText, useStopGameInstance } from '../instances/instanceActions';
import { dismissGameLaunch, startGameLaunch, useAnyGameLaunching, useGameLaunch } from '../instances/launchController';

interface GameHomePageProps {
  gameId: string;
  /** The game's short name ("Skyrim SE"). */
  gameName: string;
  onOpenInstance: (instanceId: string) => void;
  onOpenInstances: () => void;
  onOpenBrowse: () => void;
  onOpenGuide: () => void;
}

type Loaded<T> = { status: 'loading' } | { status: 'failed' } | { status: 'loaded'; value: T };

/** When an instance was last played, or null if never. */
function lastPlayed(card: InstanceCard): Date | null {
  return parseStoredTime(card.lastPlayedAt ?? card.lastLaunchedAt);
}

/** Most recently played first; never-played instances after, in the list's order. */
function byRecentPlay(cards: readonly InstanceCard[]): InstanceCard[] {
  return [...cards].sort((a, b) => (lastPlayed(b)?.getTime() ?? 0) - (lastPlayed(a)?.getTime() ?? 0));
}

export function GameHomePage({ gameId, gameName, onOpenInstance, onOpenInstances, onOpenBrowse, onOpenGuide }: GameHomePageProps) {
  const [instances, setInstances] = useState<Loaded<InstanceCard[]>>({ status: 'loading' });
  const [catalog, setCatalog] = useState<Loaded<CatalogEntry[]>>({ status: 'loading' });
  const latest = useRef(0);

  const loadInstances = useCallback((quiet = false) => {
    const request = ++latest.current;
    if (!quiet) setInstances({ status: 'loading' });
    listGameInstances(gameId)
      .then((list) => {
        if (request === latest.current) setInstances({ status: 'loaded', value: list.instances });
      })
      .catch(() => {
        if (request === latest.current && !quiet) setInstances({ status: 'failed' });
      });
  }, [gameId]);

  useEffect(() => {
    loadInstances();
  }, [loadInstances]);

  useEffect(() => {
    let cancelled = false;
    setCatalog({ status: 'loading' });
    listCatalogEntries(gameId)
      .then((listing) => {
        if (!cancelled) setCatalog({ status: 'loaded', value: listing.entries });
      })
      .catch(() => {
        if (!cancelled) setCatalog({ status: 'failed' });
      });
    return () => {
      cancelled = true;
    };
  }, [gameId]);

  // A game that closes turns the card back from Running.
  useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    onInstanceExited(() => loadInstances(true))
      .then((stop) => {
        if (cancelled) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [loadInstances]);

  const sorted = useMemo(() => (instances.status === 'loaded' ? byRecentPlay(instances.value) : []), [instances]);
  // A running instance is the one to come back to; otherwise the one played last.
  const hero = sorted.find((card) => card.running) ?? sorted[0] ?? null;
  const others = sorted.filter((card) => card !== hero).slice(0, 6);

  const startImport = () => {
    requestGameImport(gameId);
    onOpenInstances();
  };

  return (
    <div className="space-y-6" data-testid="game-home-page">
      <HomeHero description={`Your ${gameName} modding dashboard.`} />

      {instances.status === 'loading' ? (
        <div className="rounded-xl border border-border bg-card p-6 space-y-2" data-testid="game-home-loading">
          <div className="h-5 w-32 bg-muted animate-pulse rounded-sm" />
          <div className="h-4 w-48 bg-muted animate-pulse rounded-sm" />
        </div>
      ) : instances.status === 'failed' ? (
        <div role="alert" className="rounded-xl border border-destructive/50 bg-destructive/10 p-6 space-y-3">
          <h3 className="font-semibold">Your {gameName} instances could not be read</h3>
          <button
            type="button"
            onClick={() => loadInstances()}
            className="rounded-lg border border-border bg-card px-4 py-2 text-sm font-medium hover:bg-accent"
          >
            Try again
          </button>
        </div>
      ) : hero === null ? (
        <div className="rounded-xl border border-border bg-card p-6" data-testid="game-home-welcome">
          <h3 className="text-lg font-semibold mb-2">Welcome to {gameName} in Agora</h3>
          <p className="text-sm text-muted-foreground mb-4">
            No instances yet. Create one from your {gameName} install to start playing, or bring in a Mod Organizer 2
            setup you already have.
          </p>
          <div className="flex flex-wrap gap-2">
            <PageHeroAction primary icon={Plus} onClick={onOpenInstances}>
              Create an instance
            </PageHeroAction>
            <PageHeroAction icon={Download} onClick={startImport}>
              Import from Mod Organizer 2
            </PageHeroAction>
          </div>
        </div>
      ) : (
        <ContinuePlayingCard card={hero} onOpen={() => onOpenInstance(hero.instanceId)} onChanged={() => loadInstances(true)} />
      )}

      {others.length > 0 && (
        <section aria-labelledby="game-home-recent" className="space-y-2">
          <div className="flex items-center justify-between gap-2">
            <h2 id="game-home-recent" className="text-sm font-semibold">Jump back in</h2>
            <button type="button" onClick={onOpenInstances} className="text-xs font-medium text-primary hover:underline">
              All instances
            </button>
          </div>
          <ul className="grid grid-cols-[repeat(auto-fill,minmax(14rem,1fr))] gap-2">
            {others.map((card) => {
              const played = lastPlayed(card);
              return (
                <li key={card.instanceId}>
                  <button
                    type="button"
                    onClick={() => onOpenInstance(card.instanceId)}
                    title={`Open ${card.name}`}
                    className="flex w-full items-center gap-3 rounded-lg border border-border bg-card px-3 py-2 text-left hover:bg-accent"
                  >
                    <GameInstanceTile instanceId={card.instanceId} name={card.name} size={32} />
                    <span className="min-w-0">
                      <span className="block truncate text-sm font-medium">{card.name}</span>
                      <span className="block truncate text-xs text-muted-foreground">
                        {card.running ? 'Running now' : played ? `Played ${timeSince(played)}` : 'Never played'}
                      </span>
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </section>
      )}

      <GuideCard onOpenGuide={onOpenGuide} tour={false} />

      <CommunityCards />

      <CatalogStrip gameName={gameName} catalog={catalog} onOpenBrowse={onOpenBrowse} />
    </div>
  );
}

/** Minecraft's "Continue playing" card, for a game instance: play it, or stop it while it runs. */
function ContinuePlayingCard({ card, onOpen, onChanged }: { card: InstanceCard; onOpen: () => void; onChanged: () => void }) {
  const launch = useGameLaunch(card.instanceId);
  const anyLaunching = useAnyGameLaunching();
  const stopInstance = useStopGameInstance();
  const [stopping, setStopping] = useState(false);
  const [message, setMessage] = useState<{ tone: 'info' | 'danger'; text: string } | null>(null);
  const played = lastPlayed(card);
  const playTime = gameKitCapabilities(card.capabilities).playTime ? playedText(card.playTimeMs) : null;
  const { reply, error } = launch;

  const play = async () => {
    setMessage(null);
    const answer = await startGameLaunch(card.instanceId);
    // A plain "it started" needs no banner: the card turns to Running when the list reloads.
    if (answer?.status === 'launched' && answer.notice === null && answer.findings.length === 0) {
      dismissGameLaunch(card.instanceId);
    }
    onChanged();
  };

  const stop = async () => {
    setMessage(null);
    try {
      const outcome = await stopInstance(card.instanceId, card.name, () => setStopping(true));
      if (outcome.status === 'done') setMessage({ tone: 'info', text: outcome.message });
      if (outcome.status === 'failed') setMessage({ tone: 'danger', text: outcome.message });
      if (outcome.status !== 'cancelled') onChanged();
    } finally {
      setStopping(false);
    }
  };

  const blocked = anyLaunching && !launch.busy;

  return (
    <div className="rounded-xl border border-border bg-card p-6" data-testid="game-home-continue">
      <div className="flex items-start gap-4">
        <GameInstanceTile instanceId={card.instanceId} name={card.name} size={56} />
        <div className="min-w-0 flex-1">
          <h3 className="text-lg font-semibold mb-1">{card.name}</h3>
          <div className="mb-2 flex flex-wrap gap-1.5">
            <InstanceChips store={card.store} version={card.version} deployment={card.deployment} pinned={card.pinned} />
          </div>
          <p className="text-xs text-muted-foreground mb-4">
            {card.running ? (
              <RunningIndicator>Running now</RunningIndicator>
            ) : played ? (
              `${timeSince(played)}${playTime === null ? '' : ` · ${playTime}`}`
            ) : (
              'Not launched yet'
            )}
          </p>
        </div>
      </div>

      <div className="space-y-2">
        {error !== null && (
          <InstanceCardBanner tone="danger" title={error} onDismiss={() => dismissGameLaunch(card.instanceId)} />
        )}
        {reply?.status === 'refused' && (
          <InstanceCardBanner
            tone="danger"
            title="The launch checks stopped this launch"
            body="Nothing was started. The instance page says what to fix."
            action={{ label: 'Review', ariaLabel: `Review ${card.name}`, onSelect: onOpen }}
            onDismiss={() => dismissGameLaunch(card.instanceId)}
          />
        )}
        {reply?.status === 'vfs_unavailable' && (
          <InstanceCardBanner
            tone="warn"
            title="The virtual file system could not start"
            body="Nothing was started. Agora asks how to run it instead on the instance page."
            action={{ label: 'Review', ariaLabel: `Review ${card.name}`, onSelect: onOpen }}
          />
        )}
        {reply?.status === 'launched' && (
          <InstanceCardBanner
            tone="info"
            title="The game is starting"
            body={reply.notice ?? `Launched past ${reply.findings.length === 1 ? 'one check' : `${reply.findings.length} checks`}.`}
            onDismiss={() => dismissGameLaunch(card.instanceId)}
          />
        )}
        {message !== null && (
          <p role={message.tone === 'danger' ? 'alert' : 'status'} className={message.tone === 'danger' ? 'text-sm text-destructive' : 'text-sm text-muted-foreground'}>
            {message.text}
          </p>
        )}
      </div>

      <div className="mt-2 flex flex-wrap gap-2">
        {card.running ? (
          <button
            type="button"
            onClick={() => void stop()}
            disabled={stopping}
            className="inline-flex items-center gap-2 rounded-lg bg-destructive px-5 py-2 text-sm font-medium text-destructive-foreground hover:bg-destructive/90 disabled:cursor-not-allowed disabled:opacity-50"
          >
            <Square className="h-4 w-4" aria-hidden="true" />
            {stopping ? 'Stopping…' : 'Stop'}
          </button>
        ) : (
          <button
            type="button"
            onClick={() => void play()}
            disabled={launch.busy || blocked}
            title={blocked ? 'Another instance is starting. Wait for it to finish.' : undefined}
            className="inline-flex items-center gap-2 rounded-lg bg-primary px-5 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:cursor-not-allowed disabled:opacity-50"
          >
            <Play className="h-4 w-4" aria-hidden="true" />
            {launch.busy ? 'Starting…' : `Play ${card.name}`}
          </button>
        )}
        <button
          type="button"
          onClick={onOpen}
          className="rounded-lg border border-border bg-card px-5 py-2 text-sm font-medium hover:bg-accent"
        >
          Open instance
        </button>
      </div>
    </div>
  );
}

/** Minecraft's "Compatible recommendations", filled with the game's curated catalog. */
function CatalogStrip({ gameName, catalog, onOpenBrowse }: {
  gameName: string;
  catalog: Loaded<CatalogEntry[]>;
  onOpenBrowse: () => void;
}) {
  if (catalog.status === 'loading') return null;
  const entries = catalog.status === 'loaded' ? catalog.value : [];
  if (entries.length === 0) {
    return (
      <div className="rounded-xl border border-dashed border-border bg-card p-6 text-center" data-testid="game-home-catalog-empty">
        <Boxes className="mx-auto h-5 w-5 text-muted-foreground" aria-hidden="true" />
        <p className="mt-2 text-muted-foreground">
          {catalog.status === 'failed'
            ? `The ${gameName} catalog could not be read right now.`
            : `No ${gameName} mods in the catalog yet. Browse lists them as the Agora team adds them.`}
        </p>
        <button onClick={onOpenBrowse} className="mt-3 rounded-lg border border-border px-4 py-2 text-sm font-medium hover:bg-accent">
          Open Browse
        </button>
      </div>
    );
  }
  return (
    <div className="rounded-xl border border-border bg-card p-4 space-y-3" data-testid="game-home-catalog">
      <div>
        <h4 className="font-semibold text-sm">From the catalog</h4>
        <p className="text-xs text-muted-foreground">Mods the Agora team picked for {gameName}.</p>
      </div>
      <div className="grid gap-2 md:grid-cols-3">
        {entries.slice(0, 3).map((entry) => {
          const needs = entry.compatibility.flatMap((compat) => compat.requires).map((req) => req.framework);
          return (
            <button
              key={entry.id}
              type="button"
              onClick={onOpenBrowse}
              className="rounded-lg border border-border bg-muted p-3 text-left hover:bg-accent"
            >
              <span className="block text-sm font-medium">{entry.name}</span>
              <span className="mt-1 block text-xs text-muted-foreground line-clamp-2">
                {entry.description ?? (entry.author ? `by ${entry.author}` : 'Curated by the Agora team.')}
              </span>
              <span className="mt-2 block text-[11px] text-primary">
                {needs.length > 0 ? `Needs ${[...new Set(needs)].join(', ')}` : 'Curated'}
              </span>
            </button>
          );
        })}
      </div>
      <button onClick={onOpenBrowse} className="inline-flex items-center gap-2 rounded-lg border border-border px-4 py-2 text-sm font-medium hover:bg-accent">
        Browse all {gameName} mods
        <ArrowRight className="h-4 w-4" aria-hidden="true" />
      </button>
    </div>
  );
}
