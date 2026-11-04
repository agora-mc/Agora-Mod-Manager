/**
 * Browse for a game other than Minecraft: Minecraft's Browse page (`pages/Browse.tsx`) with this
 * game's catalog in its results list. The same header, the same "for an instance" card, the same
 * search box and the same result cards (`components/browse/browse-cards.css`).
 *
 * The results are the game's curated catalog. A game has no content sources (providers) yet, so
 * a game with no catalog entries says so and points to Settings, where plugins that add sources
 * are installed. Preview shows the file Agora would fetch and the checks it would make; Install
 * fetches, checks, stores and places it in the chosen instance. An archive with an installer
 * opens the FOMOD wizard (the content slice's component, imported from its folder).
 */

import { useEffect, useMemo, useRef, useState } from 'react';
import { Boxes, Compass, ExternalLink, Library as LibraryGlyph, Puzzle } from 'lucide-react';
import { EmptyState, PageHeroAction } from '@/components/instance-kit';
import { BrowseIcon } from '@/components/browse/BrowseIcon';
import { CuratedBadge } from '@/components/browse/BrowseContextLabels';
import '@/components/browse/browse-cards.css';
import { formatBytes } from '@/components/installed-content/contentTableState';
import { Switch } from '@/components/ui/switch';
import { formatError, openExternalUrl } from '@/lib/tauri';
import { openSettingsSection } from '@/lib/settingsSection';
import { isGamesError } from '../shared/types';
import { runtimeText } from '../instances/instanceIdentity';
import { FomodWizard } from '../fomod';
import {
  installCatalogEntry,
  listCatalogEntries,
  type CatalogEntry,
  type CatalogInstallReply,
  type CatalogListing,
} from './api';
import { hashOutcome } from './hashOutcome';

/** The core's code for a file that differs from the curator's pin or an earlier install. */
const NEEDS_INSTALL_ANYWAY = 'ERR_HASH_CONFIRMATION_REQUIRED';

/**
 * The "Install anyway" switch. What it does (agora-core `catalog_install`, `InstallRequest`):
 * accept a file whose SHA-256 differs from the curator's pin for that release file, or from the
 * hash it had when this release was installed before. It never accepts a file that differs from
 * the hash its source published (GitHub's digest, or a direct download's manifest hash), and it
 * does not touch the required-framework check.
 */
export const INSTALL_ANYWAY_COPY = {
  label: 'Install anyway',
  description:
    'Use the file even if it has changed since the Agora team checked it, or since you last installed it. '
    + 'Agora still stops if the download doesn’t match the checksum its source published, because that means it was damaged or changed.',
} as const;

type ListState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; listing: CatalogListing };

type EntryState =
  | { status: 'busy'; dryRun: boolean }
  | { status: 'error'; message: string; canInstallAnyway: boolean }
  | { status: 'done'; reply: CatalogInstallReply };

interface GameBrowsePageProps {
  gameId: string;
  /** The game's short name ("Skyrim SE"). */
  gameName: string;
  /** Open this game's Library (the hero's "Your Library"). */
  onOpenLibrary?: () => void;
  /** Open this game's My Instances, to make the first instance. */
  onOpenInstances?: () => void;
}

/** True when an entry matches every word of the search, in its name, author or description. */
function matches(entry: CatalogEntry, query: string): boolean {
  const words = query.toLocaleLowerCase().split(/\s+/).filter(Boolean);
  if (words.length === 0) return true;
  const text = [entry.name, entry.author ?? '', entry.description ?? ''].join(' ').toLocaleLowerCase();
  return words.every((word) => text.includes(word));
}

export function GameBrowsePage({ gameId, gameName, onOpenLibrary, onOpenInstances }: GameBrowsePageProps) {
  const [list, setList] = useState<ListState>({ status: 'loading' });
  const [instanceId, setInstanceId] = useState('');
  const [installAnyway, setInstallAnyway] = useState(false);
  const [results, setResults] = useState<Record<string, EntryState>>({});
  const [query, setQuery] = useState('');
  // Only the newest list request may answer: a slow reply for the previous game must not land.
  const latest = useRef(0);

  const load = () => {
    const request = ++latest.current;
    setList({ status: 'loading' });
    listCatalogEntries(gameId)
      .then((listing) => {
        if (request !== latest.current) return;
        setList({ status: 'loaded', listing });
        setInstanceId((current) => current || listing.instances[0]?.instanceId || '');
      })
      .catch((error: unknown) => {
        if (request === latest.current) setList({ status: 'error', message: formatError(error) });
      });
  };

  useEffect(() => {
    // The selection belongs to the game: a game switch must not keep the previous game's instance.
    setInstanceId('');
    setResults({});
    load();
    // Loaded again only when the game changes; "Retry" calls load().
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [gameId]);

  const act = (entry: CatalogEntry, dryRun: boolean, anyway = installAnyway) => {
    setResults((current) => ({ ...current, [entry.id]: { status: 'busy', dryRun } }));
    installCatalogEntry({ gameId, instanceId, entryId: entry.id, installAnyway: anyway, dryRun })
      .then((reply) => setResults((current) => ({ ...current, [entry.id]: { status: 'done', reply } })))
      .catch((error: unknown) =>
        setResults((current) => ({
          ...current,
          [entry.id]: {
            status: 'error',
            message: formatError(error),
            canInstallAnyway: isGamesError(error) && error.coreCode === NEEDS_INSTALL_ANYWAY,
          },
        })),
      );
  };

  const entries = useMemo(() => (list.status === 'loaded' ? list.listing.entries : []), [list]);
  const instances = list.status === 'loaded' ? list.listing.instances : [];
  const shown = useMemo(() => entries.filter((entry) => matches(entry, query)), [entries, query]);
  const openPlugins = () => openSettingsSection({ tab: 'services', page: 'plugins' });

  return (
    <div className="space-y-6" data-testid="game-browse-page">
      <section className="agora-hero compact">
        <div className="flex items-center gap-3 mb-2">
          <h2 className="text-2xl font-bold">Browse</h2>
          {onOpenLibrary && (
            <button
              type="button"
              onClick={onOpenLibrary}
              className="ml-auto rounded-lg border border-border px-4 py-2 text-sm font-medium hover:bg-accent"
              data-testid="open-library"
            >
              Your Library
            </button>
          )}
        </div>
        <p className="text-muted-foreground">
          Mods the Agora team have personally reviewed and selected for the catalog.
        </p>
      </section>

      {list.status === 'loaded' && entries.length > 0 && (
        <section className="rounded-xl border border-border bg-card p-4" data-testid="catalog-target">
          {instances.length === 0 ? (
            <div className="flex flex-wrap items-center justify-between gap-3" data-testid="catalog-no-instance">
              <div>
                <p className="text-sm font-semibold">Install into</p>
                <p className="mt-1 text-xs text-muted-foreground">
                  Mods are installed into an instance, and you have no {gameName} instance yet. Create an instance first.
                </p>
              </div>
              {onOpenInstances && (
                <PageHeroAction icon={Boxes} onClick={onOpenInstances}>
                  Go to My Instances
                </PageHeroAction>
              )}
            </div>
          ) : (
            <>
              <label className="block text-sm font-semibold" htmlFor="catalog-install-into">
                Install into
              </label>
              <p className="mt-1 text-xs text-muted-foreground">
                Install and Preview use this instance. Agora checks that each mod fits its game version first.
              </p>
              <select
                id="catalog-install-into"
                value={instanceId}
                onChange={(event) => setInstanceId(event.target.value)}
                className="mt-3 w-full rounded-lg border border-input bg-background px-3 py-2 text-sm"
              >
                {instances.map((instance) => (
                  <option key={instance.instanceId} value={instance.instanceId}>
                    {instance.name}
                  </option>
                ))}
              </select>
              <div className="mt-4 flex items-start gap-3 border-t border-border pt-4">
                <Switch
                  id="catalog-install-anyway"
                  aria-describedby="catalog-install-anyway-help"
                  checked={installAnyway}
                  onCheckedChange={setInstallAnyway}
                  className="mt-0.5"
                />
                <div className="min-w-0">
                  <label htmlFor="catalog-install-anyway" className="text-sm font-medium">
                    {INSTALL_ANYWAY_COPY.label}
                  </label>
                  <p id="catalog-install-anyway-help" className="mt-0.5 text-xs text-muted-foreground">
                    {INSTALL_ANYWAY_COPY.description}
                  </p>
                </div>
              </div>
            </>
          )}
        </section>
      )}

      {list.status === 'loaded' && entries.length > 0 && (
        <input
          type="text"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder={`Search ${gameName} mods…`}
          aria-label="Search mods"
          className="w-full rounded-lg border border-input bg-background px-4 py-2 text-sm focus:outline-hidden focus:ring-2 focus:ring-ring"
        />
      )}

      {list.status === 'error' && (
        <div role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm text-destructive">
          <p>Agora could not load the mods for {gameName}: {list.message}</p>
          <div className="mt-2 flex gap-2">
            <button
              type="button"
              onClick={load}
              className="rounded-lg bg-primary px-3 py-1 text-xs font-medium text-primary-foreground hover:bg-primary/90"
            >
              Retry
            </button>
          </div>
        </div>
      )}

      {list.status === 'loading' ? (
        <div role="status" className="rounded-xl border border-dashed border-border bg-card p-6 text-center text-muted-foreground">
          Loading mods…
        </div>
      ) : list.status === 'loaded' && entries.length === 0 ? (
        <EmptyState
          icon={Compass}
          testId="catalog-empty"
          title={`Nothing to browse for ${gameName} yet`}
          body={
            <>
              The Agora team hasn’t added {gameName} mods to the catalog yet, and no other source of mods is set up
              for {gameName}. Sources come from plugins, which you add in Settings. Mods you download yourself go in
              your Library.
            </>
          }
          actions={
            <>
              <PageHeroAction primary icon={Puzzle} onClick={openPlugins}>
                Open Settings &gt; Plugins
              </PageHeroAction>
              {onOpenLibrary && (
                <PageHeroAction icon={LibraryGlyph} onClick={onOpenLibrary}>
                  Your Library
                </PageHeroAction>
              )}
            </>
          }
        />
      ) : list.status === 'loaded' && shown.length === 0 ? (
        <div className="rounded-xl border border-dashed border-border bg-card p-6 text-center">
          <p className="text-muted-foreground">No mods match this search.</p>
          <button
            type="button"
            onClick={() => setQuery('')}
            className="mt-2 rounded-lg bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground hover:bg-primary/90"
          >
            Clear search
          </button>
        </div>
      ) : list.status === 'loaded' ? (
        <div className="browse-list-results-shell">
          <ul className="browse-list-results" aria-label={`${gameName} mods`}>
            {shown.map((entry) => (
              <CatalogCard
                key={entry.id}
                entry={entry}
                canInstall={instanceId !== ''}
                result={results[entry.id]}
                onPreview={() => act(entry, true)}
                onInstall={() => act(entry, false)}
                onInstallAnyway={() => act(entry, false, true)}
              />
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

interface CatalogCardProps {
  entry: CatalogEntry;
  canInstall: boolean;
  result: EntryState | undefined;
  onPreview: () => void;
  onInstall: () => void;
  /** Install this one past a changed file (the answer to "it differs from the curator's pin"). */
  onInstallAnyway: () => void;
}

/** Where an entry's file comes from, in a player's words. */
function sourceText(entry: CatalogEntry): string {
  const from = entry.downloadStrategy === 'github_release' ? 'Downloads from GitHub' : 'Direct download';
  return entry.author ? `by ${entry.author} · ${from}` : from;
}

/** The game versions an entry is for: "Steam 1.6.1170", one per store. */
function fitsText(entry: CatalogEntry): string[] {
  const fits = entry.compatibility.flatMap((compat) => {
    const versions = compat.gameVersions.join(', ') || 'any version';
    const stores = compat.stores.length > 0 ? compat.stores : [null];
    return stores.map((store) => runtimeText(store, versions) ?? versions);
  });
  return [...new Set(fits)];
}

/** One catalog entry, on Minecraft's Browse list card. */
function CatalogCard({ entry, canInstall, result, onPreview, onInstall, onInstallAnyway }: CatalogCardProps) {
  const busy = result?.status === 'busy';
  const previewing = result?.status === 'busy' && result.dryRun;
  const installing = result?.status === 'busy' && !result.dryRun;
  const needs = entry.compatibility
    .flatMap((compat) => compat.requires)
    .map((req) => (req.minVersion ? `${req.framework} ${req.minVersion} or newer` : req.framework));
  const fits = fitsText(entry);
  return (
    <li className="browse-list-card" data-testid="catalog-entry">
      <div className="browse-list-card__layout">
        <BrowseIcon iconUrl={null} name={entry.name} className="browse-list-card__icon" />
        <div className="browse-list-card__main">
          <div className="browse-card-title-row">
            <h3 className="browse-card-title">{entry.name}</h3>
            <CuratedBadge />
          </div>
          <p className="browse-card-source">{sourceText(entry)}</p>
          {entry.description && <p className="browse-list-card__description">{entry.description}</p>}
          {fits.length > 0 && (
            <div className="browse-category-list mt-1.5" aria-label="Works with">
              {fits.map((fit) => <span key={fit}>{fit}</span>)}
            </div>
          )}
          {needs.length > 0 && <p className="browse-versions mt-1">Needs {[...new Set(needs)].join(', ')}</p>}
        </div>
        <aside className="browse-list-card__side">
          <div className="browse-list-card__actions">
            <div className="browse-card-action-row browse-card-action-row--pair">
              <button type="button" className="browse-primary-action" onClick={onInstall} disabled={!canInstall || busy}>
                {installing ? 'Installing…' : 'Install'}
              </button>
              <button type="button" className="browse-secondary-action" onClick={onPreview} disabled={!canInstall || busy}>
                {previewing ? 'Checking…' : 'Preview'}
              </button>
            </div>
            {entry.pageUrl && (
              <button
                type="button"
                className="browse-source-link"
                // `target="_blank"` is inert inside the Tauri webview, so the link goes through the OS opener.
                onClick={() => void openExternalUrl(entry.pageUrl!)}
              >
                View source <ExternalLink aria-hidden size={12} />
              </button>
            )}
          </div>
        </aside>
      </div>

      {result?.status === 'error' && (
        <div role="alert" className="mt-3 space-y-2 rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          <p className="text-destructive">Not installed: {result.message}</p>
          {result.canInstallAnyway && (
            <>
              <p className="text-muted-foreground">
                The file has changed since the Agora team checked it, or since you last installed it. Install it only if
                you trust where it came from.
              </p>
              <button
                type="button"
                onClick={onInstallAnyway}
                className="rounded-lg border border-border bg-card px-3 py-1.5 text-xs font-medium hover:bg-accent"
              >
                Install anyway
              </button>
            </>
          )}
        </div>
      )}

      {result?.status === 'done' && (
        <div className="mt-3 border-t border-border pt-3">
          <CatalogResult reply={result.reply} />
        </div>
      )}
    </li>
  );
}

function CatalogResult({ reply }: { reply: CatalogInstallReply }) {
  // The wizard's state. This component only renders once the install is done, and a new install
  // unmounts it first, so each run starts with the wizard open.
  const [wizard, setWizard] = useState<'open' | 'closed' | 'installed'>('open');
  const size = reply.size !== null ? ` (${formatBytes(reply.size)})` : '';
  const frameworkNotes = reply.frameworks.map((framework) =>
    framework.foundVersion
      ? `${framework.name} ${framework.foundVersion}`
      : `${framework.name} is installed, but its version cannot be read`,
  );
  const warnings = reply.warnings.map((warning) => (
    <li key={warning} className="text-amber-700 dark:text-amber-300">
      {warning}
    </li>
  ));

  switch (reply.state) {
    case 'dry_run':
      return (
        <div className="space-y-1 text-sm" data-testid="catalog-preview">
          <p>
            Would download {reply.fileName}{size} from {reply.source}. Nothing has been downloaded.
          </p>
          {frameworkNotes.length > 0 && <p className="text-muted-foreground">Found: {frameworkNotes.join('; ')}.</p>}
          {warnings.length > 0 && <ul className="space-y-1">{warnings}</ul>}
        </div>
      );
    case 'already_installed':
      return (
        <p className="text-sm" data-testid="catalog-already">
          {reply.sameRelease
            ? `Already in this instance${reply.installedRelease ? ` (version ${reply.installedRelease})` : ''}. Nothing changed.`
            : `This instance has version ${reply.installedRelease ?? 'an earlier one'}; the catalog offers ${reply.release ?? 'another version'}. Nothing changed.`}
        </p>
      );
    case 'placed': {
      const outcome = hashOutcome(reply.hash);
      return (
        <div className="space-y-2 text-sm" data-testid="catalog-placed">
          <p>
            Installed {reply.itemName} into {reply.mountPath}.
          </p>
          <div className="rounded-lg border border-border p-3" data-testid="catalog-hash">
            <p className={outcome.tone === 'verified' ? 'font-medium text-emerald-700 dark:text-emerald-300' : 'font-medium'}>
              {outcome.headline}
            </p>
            <p className="text-muted-foreground">{outcome.basis}</p>
            {outcome.acceptedPast && <p className="text-amber-700 dark:text-amber-300">{outcome.acceptedPast}</p>}
            <p className="font-mono text-xs text-muted-foreground">sha-256 {reply.hash.sha256.slice(0, 16)}…</p>
          </div>
          {warnings.length > 0 && <ul className="space-y-1">{warnings}</ul>}
        </div>
      );
    }
    case 'needs_installer':
      return (
        <div className="space-y-3 text-sm" data-testid="catalog-needs-installer">
          <p>
            {reply.itemName} has an installer with questions. Answer them to finish the install. {reply.reason}
          </p>
          {wizard === 'open' && (
            <FomodWizard
              contentId={reply.contentItemId}
              instanceId={reply.instanceId}
              onClose={() => setWizard('closed')}
              onInstalled={() => setWizard('installed')}
            />
          )}
          {wizard === 'closed' && (
            <p data-testid="catalog-installer-closed">
              The installer was closed. The mod stays in your Library, and nothing has been added to the instance yet.
            </p>
          )}
          {wizard === 'installed' && (
            <p data-testid="catalog-installer-done">The installer finished. The wizard says what was added.</p>
          )}
        </div>
      );
  }
}
