/**
 * The Library: every game's one page over the shared content store, showing the selected game's
 * items. It reads no network. Each item says where it came from, which instances use it and, for a
 * game with a plugin list, which plugins it carries. "Add to instance…" places an item into an
 * instance of its game; items that nothing ties to a game sit under their own filter, so nothing
 * is hidden or filed under the wrong game. A game's curated catalog is on its Browse page, as
 * Minecraft's is; Minecraft adds "Fill in missing details" from Modrinth.
 */

import { useCallback, useEffect, useMemo, useState } from 'react';
import { FileArchive, FolderPlus, Library as LibraryGlyph, RefreshCw, Search, Sparkles } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { useConfirm } from '@/components/ui/confirm';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { EmptyState, PageHero, PageHeroAction } from '@/components/instance-kit';
import { NewDownloadsSection } from '@/features/downloads/NewDownloadsSection';
import { downloadsForGame } from '@/features/downloads/api';
import { useNewDownloads } from '@/features/downloads/useNewDownloads';
import {
  addToStore,
  pickArchive,
  pickFolder,
  verifyStored,
  type VerifyReport,
} from '@/features/games/content/api';
import { isGamesError, MINECRAFT_GAME_ID } from '@/features/games/shared/types';
import { cn } from '@/lib/utils';
import {
  addLibraryItem,
  fillMissingLibraryDetails,
  instancesForItem,
  itemsInView,
  listLibrary,
  listLibraryInstances,
  listUnusedItems,
  removeLibraryItem,
  searchLibrary,
  type LibraryInstance,
  type LibraryItem,
  type LibraryListing,
  type LibraryView,
  type UnusedItem,
  type UnusedListing,
} from './api';
import { formatSize, plural } from './format';
import { LibraryItemCard } from './LibraryItemCard';

type LoadState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; listing: LibraryListing };

function errorMessage(error: unknown): string {
  if (isGamesError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong. Try again.';
}

export interface LibraryPageProps {
  /** The selected game: the page lists its items. */
  gameId?: string;
  /** The game's name as the sidebar shows it ("Skyrim SE"). */
  gameName?: string;
  /** Names of other games, for an item that is tied to more than one. */
  gameNames?: Record<string, string>;
}

export function LibraryPage({
  gameId = MINECRAFT_GAME_ID,
  gameName = 'Minecraft',
  gameNames = {},
}: LibraryPageProps) {
  const { confirm } = useConfirm();
  const isMinecraft = gameId === MINECRAFT_GAME_ID;
  const [state, setState] = useState<LoadState>({ status: 'loading' });
  const [instances, setInstances] = useState<LibraryInstance[]>([]);
  const [instanceWarnings, setInstanceWarnings] = useState<string[]>([]);
  const [query, setQuery] = useState('');
  const [view, setView] = useState<LibraryView>('game');
  const [addTarget, setAddTarget] = useState<LibraryItem | null>(null);
  const [pickedInstance, setPickedInstance] = useState('');
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [reports, setReports] = useState<Record<string, VerifyReport>>({});
  const [unused, setUnused] = useState<UnusedListing | null>(null);
  const [unusedOpen, setUnusedOpen] = useState(false);
  const [unusedSelected, setUnusedSelected] = useState<Set<string>>(new Set());
  const [unusedConfirming, setUnusedConfirming] = useState(false);
  const [unusedBusy, setUnusedBusy] = useState(false);
  const [unusedError, setUnusedError] = useState<string | null>(null);
  const [fillBusy, setFillBusy] = useState(false);
  const [fillWarnings, setFillWarnings] = useState<string[]>([]);
  const { rows: downloadRows } = useNewDownloads();

  const gameLabel = useCallback(
    (id: string) => (id === gameId ? gameName : gameNames[id] ?? (id === MINECRAFT_GAME_ID ? 'Minecraft' : id)),
    [gameId, gameName, gameNames],
  );

  const load = useCallback(async () => {
    setState((current) => (current.status === 'loaded' ? current : { status: 'loading' }));
    try {
      const [listing, options, unusedListing] = await Promise.all([
        listLibrary(),
        listLibraryInstances(),
        listUnusedItems(),
      ]);
      setState({ status: 'loaded', listing });
      setInstances(options.instances);
      setInstanceWarnings(options.warnings);
      setUnused(unusedListing);
    } catch (error) {
      // Not "empty": the list did not come back, so the page says so and offers a retry.
      setState({ status: 'error', message: errorMessage(error) });
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const items = useMemo(() => (state.status === 'loaded' ? state.listing.items : []), [state]);
  const gameItems = useMemo(() => itemsInView(items, gameId, 'game'), [items, gameId]);
  const untiedItems = useMemo(() => itemsInView(items, gameId, 'untied'), [items, gameId]);
  const viewItems = view === 'untied' ? untiedItems : gameItems;
  const visible = useMemo(() => searchLibrary(viewItems, query), [viewItems, query]);
  const unusedInView = useMemo(
    () => (unused ? itemsInView(unused.items, gameId, view) : []),
    [unused, gameId, view],
  );
  const unusedSize = unusedInView.reduce((sum, item) => sum + item.totalSize, 0);
  const gameSize = gameItems.reduce((sum, item) => sum + item.totalSize, 0);
  const newDownloads = downloadsForGame(downloadRows, gameId).length;
  const viewName = view === 'untied' ? 'Not tied to a game' : gameName;

  // An untied item may go to any instance of the game whose Library is open; a tied one to the
  // instances of the games it is tied to.
  const candidatesFor = useCallback(
    (item: LibraryItem) =>
      item.gameIds.length === 0
        ? instances.filter((instance) => instance.gameId === gameId)
        : instancesForItem(item, instances),
    [instances, gameId],
  );

  const clearMessages = () => {
    setNotice(null);
    setActionError(null);
  };

  const openAdd = (item: LibraryItem) => {
    const candidates = candidatesFor(item);
    setAddTarget(item);
    setPickedInstance(candidates.length === 1 ? candidates[0].instanceId : '');
    clearMessages();
  };

  const closeAdd = () => {
    setAddTarget(null);
    setPickedInstance('');
  };

  const confirmAdd = async () => {
    if (!addTarget) return;
    const instance = instances.find((candidate) => candidate.instanceId === pickedInstance);
    if (!instance) return;
    setBusy('add-to-instance');
    clearMessages();
    try {
      const outcome = await addLibraryItem(addTarget, instance);
      if (outcome.status === 'needs_installer') {
        setNotice(`${addTarget.name} has an installer. Open ${instance.name}'s content to answer its questions.`);
      } else {
        setNotice(`Added ${addTarget.name} to ${instance.name}.`);
      }
      closeAdd();
      await load();
    } catch (error) {
      setActionError(errorMessage(error));
    } finally {
      setBusy(null);
    }
  };

  const addFrom = async (pick: () => Promise<string | null>, what: string) => {
    clearMessages();
    setBusy('add-to-library');
    try {
      const path = await pick();
      if (path === null) return;
      const added = await addToStore(path, null, gameId);
      setNotice(
        added.status === 'added'
          ? `Added ${added.item.name} (${plural(added.item.fileCount, 'file')}).`
          : `${added.item.name} was already in your library, so nothing new was stored.`,
      );
      setView('game');
      await load();
    } catch (error) {
      setActionError(`Could not add the ${what}: ${errorMessage(error)}`);
    } finally {
      setBusy(null);
    }
  };

  const check = async (item: LibraryItem, full: boolean) => {
    clearMessages();
    setBusy(item.itemId);
    try {
      const report = await verifyStored(item.itemId, full);
      setReports((current) => ({ ...current, [item.itemId]: report }));
    } catch (error) {
      setActionError(`Could not check ${item.name}: ${errorMessage(error)}`);
    } finally {
      setBusy(null);
    }
  };

  const remove = async (item: { itemId: string; name: string }) => {
    const confirmed = await confirm({
      title: `Remove ${item.name} from your library?`,
      body: 'Its files are deleted from your library. Instances keep the files they already have. This cannot be undone.',
      confirmLabel: 'Remove',
      tone: 'danger',
    });
    if (!confirmed) return;
    clearMessages();
    setBusy(item.itemId);
    try {
      await removeLibraryItem(item.itemId);
      setNotice(`Removed ${item.name}.`);
      setReports((current) => {
        const next = { ...current };
        delete next[item.itemId];
        return next;
      });
      await load();
    } catch (error) {
      // The core refuses while an instance uses the item, and its message names the instances.
      setActionError(`Could not remove ${item.name}: ${errorMessage(error)}`);
    } finally {
      setBusy(null);
    }
  };

  const fillDetails = async () => {
    setFillBusy(true);
    clearMessages();
    setFillWarnings([]);
    try {
      const report = await fillMissingLibraryDetails();
      setNotice(
        report.considered === 0
          ? 'Every item already has its details.'
          : `Filled in ${report.updated} of ${report.considered} item(s).`,
      );
      setFillWarnings([...report.warnings, ...report.skipped.map((skip) => `${skip.name}: ${skip.reason}`)]);
      await load();
    } catch (error) {
      setActionError(errorMessage(error));
    } finally {
      setFillBusy(false);
    }
  };

  const openUnused = () => {
    setUnusedOpen(true);
    setUnusedSelected(new Set());
    setUnusedConfirming(false);
    setUnusedError(null);
  };

  const closeUnused = () => {
    if (unusedBusy) return;
    setUnusedOpen(false);
    setUnusedSelected(new Set());
    setUnusedConfirming(false);
    setUnusedError(null);
  };

  const toggleUnused = (itemId: string) => {
    setUnusedSelected((previous) => {
      const next = new Set(previous);
      if (next.has(itemId)) next.delete(itemId);
      else next.add(itemId);
      return next;
    });
    setUnusedConfirming(false);
  };

  const confirmRemoveUnused = async () => {
    const chosen = unusedInView.filter((item) => unusedSelected.has(item.itemId));
    if (chosen.length === 0) return;
    setUnusedBusy(true);
    setUnusedError(null);
    const failures: string[] = [];
    let removed = 0;
    for (const item of chosen) {
      try {
        await removeLibraryItem(item.itemId);
        removed += 1;
      } catch (error) {
        failures.push(`${item.name}: ${errorMessage(error)}`);
      }
    }
    setUnusedBusy(false);
    if (failures.length > 0) {
      setUnusedError(failures.join(' '));
      await load();
      return;
    }
    setNotice(`Removed ${plural(removed, 'item')}.`);
    setUnusedOpen(false);
    setUnusedSelected(new Set());
    setUnusedConfirming(false);
    await load();
  };

  const addCandidates = addTarget ? candidatesFor(addTarget) : [];
  const selectedUnused = unusedSelected.size;
  const addButtons = !isMinecraft && (
    <>
      <PageHeroAction
        icon={FileArchive}
        primary
        onClick={() => void addFrom(pickArchive, 'archive')}
        disabled={busy !== null}
      >
        Add an archive…
      </PageHeroAction>
      <PageHeroAction icon={FolderPlus} onClick={() => void addFrom(pickFolder, 'folder')} disabled={busy !== null}>
        Add a folder…
      </PageHeroAction>
    </>
  );

  return (
    <div className="space-y-6" data-testid="library-page">
      <PageHero
        icon={LibraryGlyph}
        title="Library"
        description={
          isMinecraft
            ? 'Mods, packs and files you have installed or added for Minecraft, with their saved descriptions. This list works without the internet.'
            : `Mods saved for ${gameName}, each is stored once and shared by your instances. Mods on this list can be reinstalled without internet.`
        }
        chips={[
          state.status === 'loaded' && gameItems.length > 0 && plural(gameItems.length, 'item'),
          state.status === 'loaded' && gameItems.length > 0 && `${formatSize(gameSize)} stored`,
          newDownloads > 0 && `${plural(newDownloads, 'new download')}`,
        ]}
        actions={
          <>
            {addButtons}
            {isMinecraft && (
              <PageHeroAction icon={Sparkles} onClick={() => void fillDetails()} disabled={fillBusy}>
                {fillBusy ? 'Filling in…' : 'Fill in missing details'}
              </PageHeroAction>
            )}
          </>
        }
      />

      <NewDownloadsSection gameId={gameId} onLibraryChanged={() => void load()} />

      {items.length > 0 && (
        <div className="flex flex-col gap-3 lg:flex-row lg:items-center">
          <label className="relative min-w-48 flex-1">
            <Search className="pointer-events-none absolute left-3 top-2.5 h-4 w-4 text-muted-foreground" aria-hidden="true" />
            <input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder={`Search your ${view === 'untied' ? '' : `${gameName} `}library…`}
              aria-label="Search your library"
              className="w-full rounded-lg border border-input bg-background py-2 pl-9 pr-3 text-sm"
            />
          </label>
          {(untiedItems.length > 0 || view === 'untied') && (
            <div
              role="group"
              aria-label="Which items"
              className="inline-flex shrink-0 rounded-lg border border-border bg-card p-1"
            >
              {([
                ['game', gameName, gameItems.length],
                ['untied', 'Not tied to a game', untiedItems.length],
              ] as const).map(([id, label, count]) => (
                <button
                  key={id}
                  type="button"
                  aria-pressed={view === id}
                  onClick={() => setView(id)}
                  className={cn(
                    'inline-flex items-center gap-2 rounded-md px-3 py-1.5 text-sm font-medium transition-colors',
                    view === id ? 'bg-primary text-primary-foreground shadow-xs' : 'text-muted-foreground hover:bg-accent',
                  )}
                >
                  {label}
                  <span
                    className={cn(
                      'rounded-full px-1.5 text-xs',
                      view === id ? 'bg-primary-foreground/20' : 'bg-muted',
                    )}
                  >
                    {count}
                  </span>
                </button>
              ))}
            </div>
          )}
          <Button variant="outline" onClick={() => void load()} className="shrink-0">
            <RefreshCw aria-hidden="true" />
            Refresh
          </Button>
        </div>
      )}

      {view === 'untied' && (
        <p className="text-sm text-muted-foreground">
          {`Nothing says which game these are for, and no instance uses them yet. Adding one to a ${gameName} instance ties it to ${gameName}.`}
        </p>
      )}

      {fillWarnings.length > 0 && (
        <div role="status" className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
          <p className="font-medium">Some details could not be filled in.</p>
          <ul className="mt-1 space-y-0.5 text-xs text-muted-foreground">
            {fillWarnings.map((warning) => <li key={warning}>{warning}</li>)}
          </ul>
        </div>
      )}
      {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
      {actionError && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          {actionError}
        </p>
      )}
      {instanceWarnings.length > 0 && (
        <div role="status" className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
          <p className="font-medium">Some instances could not be read.</p>
          <ul className="mt-1 space-y-0.5 text-xs text-muted-foreground">
            {instanceWarnings.map((warning) => <li key={warning}>{warning}</li>)}
          </ul>
        </div>
      )}

      {unused && (unusedInView.length > 0 || unused.warnings.length > 0) && (
        <div
          className="flex flex-col gap-3 rounded-xl border border-border bg-card p-4 sm:flex-row sm:items-center sm:justify-between"
          data-testid="unused-summary"
        >
          {unused.warnings.length > 0 ? (
            <div>
              <p className="text-sm font-medium">Unused items could not be checked</p>
              <p className="text-xs text-muted-foreground">
                Some instances could not be read, so nothing is offered for removal. Fix or remove those instances
                first.
              </p>
              <ul className="mt-1 space-y-0.5 text-xs text-muted-foreground">
                {unused.warnings.map((warning) => <li key={warning}>{warning}</li>)}
              </ul>
            </div>
          ) : (
            <>
              <div>
                <p className="text-sm font-medium">
                  Unused: {plural(unusedInView.length, 'item')}, {formatSize(unusedSize)}
                </p>
                <p className="text-xs text-muted-foreground">
                  No instance uses these. Removing one frees its space; instances keep the files they already have.
                </p>
              </div>
              <Button variant="outline" onClick={openUnused}>
                Review unused items
              </Button>
            </>
          )}
        </div>
      )}

      {state.status === 'loading' ? (
        <div className="rounded-xl border border-dashed border-border bg-card p-6 text-center text-muted-foreground">
          Reading your library…
        </div>
      ) : state.status === 'error' ? (
        <div
          role="alert"
          className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm"
        >
          <span>Your library could not be read: {state.message}</span>
          <Button variant="outline" size="sm" onClick={() => void load()}>
            Try again
          </Button>
        </div>
      ) : (
        <>
          {state.listing.warnings.length > 0 && (
            <div role="status" className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
              <p className="font-medium">Some saved mods could not be read.</p>
              <ul className="mt-1 space-y-0.5 text-xs text-muted-foreground">
                {state.listing.warnings.map((warning) => <li key={warning}>{warning}</li>)}
              </ul>
            </div>
          )}

          {state.listing.damaged.length > 0 && (
            <ul className="space-y-2" aria-label="Damaged mods">
              {state.listing.damaged.map((record) => (
                <li
                  key={record.itemId}
                  className="flex flex-wrap items-start justify-between gap-3 rounded-xl border border-destructive/50 bg-destructive/10 p-4"
                  data-testid="library-damaged"
                >
                  <div className="min-w-0 space-y-0.5">
                    <p className="font-medium">A saved mod cannot be read</p>
                    <p className="text-sm">{record.message}</p>
                  </div>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={() => void remove({ itemId: record.itemId, name: record.itemId })}
                    disabled={busy !== null}
                  >
                    Remove
                  </Button>
                </li>
              ))}
            </ul>
          )}

          {viewItems.length === 0 ? (
            <EmptyState
              icon={LibraryGlyph}
              title={`Nothing in your ${gameName} library yet`}
              body={
                isMinecraft
                  ? 'Mods and packs you install are kept here, ready to add to any instance, even offline.'
                  : 'Mods automatically show up here, you can add more manually with the buttons below.'
              }
              actions={addButtons || undefined}
              testId="library-empty"
            />
          ) : visible.length === 0 ? (
            <div className="rounded-xl border border-dashed border-border bg-card p-6 text-center text-muted-foreground">
              No items in {viewName} match this search.
            </div>
          ) : (
            <ul className="space-y-3" data-testid="library-list" aria-label={`${viewName} items`}>
              {visible.map((item) => {
                const candidates = candidatesFor(item);
                return (
                  <LibraryItemCard
                    key={item.itemId}
                    item={item}
                    gameId={gameId}
                    gameLabel={gameLabel}
                    addDisabledReason={
                      candidates.length === 0 ? `No ${gameLabel(item.gameIds[0] ?? gameId)} instance exists yet.` : null
                    }
                    busy={busy !== null}
                    report={reports[item.itemId]}
                    onAdd={() => openAdd(item)}
                    onCheck={(full) => void check(item, full)}
                    onRemove={() => void remove(item)}
                  />
                );
              })}
            </ul>
          )}
        </>
      )}

      <Dialog open={addTarget !== null} onOpenChange={(open) => { if (!open) closeAdd(); }}>
        <DialogContent className="w-[min(28rem,calc(100vw-2rem))]">
          <DialogHeader>
            <DialogTitle>Add to instance</DialogTitle>
            <DialogDescription>
              {addTarget ? `Choose an instance for ${addTarget.name}.` : ''}
            </DialogDescription>
          </DialogHeader>
          {addCandidates.length === 0 ? (
            <p className="text-sm text-muted-foreground">No matching instance yet. Create one first.</p>
          ) : (
            <>
              <select
                value={pickedInstance}
                onChange={(event) => setPickedInstance(event.target.value)}
                aria-label="Target instance"
                className="w-full rounded-lg border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">Choose an instance…</option>
                {addCandidates.map((instance) => (
                  <option key={instance.instanceId} value={instance.instanceId}>
                    {instance.gameId === gameId ? instance.name : `${instance.name} (${gameLabel(instance.gameId)})`}
                  </option>
                ))}
              </select>
              {actionError && (
                <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-2 text-sm">
                  {actionError}
                </p>
              )}
              <div className="flex justify-end gap-2">
                <Button variant="outline" onClick={closeAdd} disabled={busy !== null}>
                  Cancel
                </Button>
                <Button onClick={() => void confirmAdd()} disabled={busy !== null || !pickedInstance}>
                  {busy === 'add-to-instance' ? 'Adding…' : 'Add to instance'}
                </Button>
              </div>
            </>
          )}
        </DialogContent>
      </Dialog>

      <Dialog open={unusedOpen} onOpenChange={(open) => { if (!open) closeUnused(); }}>
        <DialogContent className="w-[min(34rem,calc(100vw-2rem))]">
          <DialogHeader>
            <DialogTitle>Unused items</DialogTitle>
            <DialogDescription>
              No instance uses these. Removing one deletes it from your library and frees its space. Instances keep
              the files they already have. Nothing is removed until you confirm.
            </DialogDescription>
          </DialogHeader>

          {unusedError && (
            <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-2 text-sm">
              {unusedError}
            </p>
          )}

          <ul className="max-h-80 space-y-2 overflow-auto" data-testid="unused-list">
            {unusedInView.map((item: UnusedItem) => (
              <li key={item.itemId}>
                <label className="flex items-start gap-3 rounded-lg border border-border p-3">
                  <input
                    type="checkbox"
                    checked={unusedSelected.has(item.itemId)}
                    onChange={() => toggleUnused(item.itemId)}
                    className="mt-1"
                  />
                  <span className="min-w-0 flex-1">
                    <span className="block text-sm font-medium">{item.name}</span>
                    <span className="block text-xs text-muted-foreground">{formatSize(item.totalSize)}</span>
                  </span>
                </label>
              </li>
            ))}
          </ul>

          {unusedConfirming ? (
            <div className="space-y-3">
              <p className="text-sm">
                Remove {plural(selectedUnused, 'item')}? This cannot be undone. Instances keep the files they already
                have.
              </p>
              <div className="flex justify-end gap-2">
                <Button variant="outline" onClick={() => setUnusedConfirming(false)} disabled={unusedBusy}>
                  Back
                </Button>
                <Button onClick={() => void confirmRemoveUnused()} disabled={unusedBusy}>
                  {unusedBusy ? 'Removing…' : `Remove ${plural(selectedUnused, 'item')}`}
                </Button>
              </div>
            </div>
          ) : (
            <div className="flex justify-end gap-2">
              <Button variant="outline" onClick={closeUnused} disabled={unusedBusy}>
                Cancel
              </Button>
              <Button onClick={() => setUnusedConfirming(true)} disabled={selectedUnused === 0}>
                Remove selected…
              </Button>
            </div>
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}
