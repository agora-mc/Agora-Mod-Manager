/**
 * The offline library: everything the content store holds, with the descriptions saved on disk and
 * the instances that use it. It reads no network. "Add to instance..." places an item into an
 * instance of its own game, using the Minecraft store command or the generic content command.
 */

import { useCallback, useEffect, useMemo, useState } from 'react';
import { Search } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { isGamesError } from '@/features/games/shared/types';
import {
  addLibraryItem,
  filterLibraryByGame,
  instancesForItem,
  libraryGames,
  listLibrary,
  listLibraryInstances,
  listUnusedItems,
  loadLibraryIcon,
  removeLibraryItem,
  searchLibrary,
  type LibraryInstance,
  type LibraryItem,
  type LibraryListing,
  type UnusedItem,
  type UnusedListing,
} from '@/features/library/api';

type LoadState =
  | { status: 'loading' }
  | { status: 'error'; message: string }
  | { status: 'loaded'; listing: LibraryListing };

function errorMessage(error: unknown): string {
  if (isGamesError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong. Try again.';
}

function gameLabel(gameId: string): string {
  if (gameId === 'minecraft') return 'Minecraft';
  return gameId;
}

function formatSize(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${bytes} bytes`;
}

function itemCount(count: number): string {
  return count === 1 ? '1 item' : `${count} items`;
}

function LibraryIcon({ item }: { item: LibraryItem }) {
  const [iconSrc, setIconSrc] = useState<string | null>(null);

  // The listing carries only the hash; the bytes are read when this row renders.
  useEffect(() => {
    let cancelled = false;
    setIconSrc(null);
    if (item.iconSha256) {
      void loadLibraryIcon(item.iconSha256).then((src) => {
        if (!cancelled && src) setIconSrc(src);
      });
    }
    return () => {
      cancelled = true;
    };
  }, [item.iconSha256]);

  if (iconSrc) {
    return (
      <img
        src={iconSrc}
        alt=""
        aria-hidden="true"
        className="h-12 w-12 shrink-0 rounded-lg border border-border object-cover"
      />
    );
  }
  return (
    <div
      aria-hidden="true"
      className="flex h-12 w-12 shrink-0 items-center justify-center rounded-lg border border-border bg-muted text-lg font-semibold text-muted-foreground"
    >
      {item.name.trim().charAt(0).toLocaleUpperCase() || '?'}
    </div>
  );
}

export function LibraryPage() {
  const [state, setState] = useState<LoadState>({ status: 'loading' });
  const [instances, setInstances] = useState<LibraryInstance[]>([]);
  const [instanceWarnings, setInstanceWarnings] = useState<string[]>([]);
  const [query, setQuery] = useState('');
  const [game, setGame] = useState<string | null>(null);
  const [addTarget, setAddTarget] = useState<LibraryItem | null>(null);
  const [pickedInstance, setPickedInstance] = useState('');
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [unused, setUnused] = useState<UnusedListing | null>(null);
  const [unusedOpen, setUnusedOpen] = useState(false);
  const [unusedSelected, setUnusedSelected] = useState<Set<string>>(new Set());
  const [unusedConfirming, setUnusedConfirming] = useState(false);
  const [unusedBusy, setUnusedBusy] = useState(false);
  const [unusedError, setUnusedError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setState({ status: 'loading' });
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
      setState({ status: 'error', message: errorMessage(error) });
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const items = state.status === 'loaded' ? state.listing.items : [];
  const games = useMemo(() => libraryGames(items), [items]);
  const visible = useMemo(
    () => searchLibrary(filterLibraryByGame(items, game), query),
    [items, game, query],
  );

  const openAdd = (item: LibraryItem) => {
    const candidates = instancesForItem(item, instances);
    setAddTarget(item);
    setPickedInstance(candidates.length === 1 ? candidates[0].instanceId : '');
    setActionError(null);
    setNotice(null);
  };

  const closeAdd = () => {
    setAddTarget(null);
    setPickedInstance('');
  };

  const confirmAdd = async () => {
    if (!addTarget) return;
    const instance = instances.find((candidate) => candidate.instanceId === pickedInstance);
    if (!instance) return;
    setBusy(true);
    setActionError(null);
    setNotice(null);
    try {
      const outcome = await addLibraryItem(addTarget, instance);
      if (outcome.status === 'needs_installer') {
        setNotice(
          `${addTarget.name} has an installer. Open ${instance.name}'s content to answer its questions.`,
        );
      } else {
        setNotice(`Added ${addTarget.name} to ${instance.name}.`);
      }
      closeAdd();
      await load();
    } catch (error) {
      setActionError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  const candidates = addTarget ? instancesForItem(addTarget, instances) : [];

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
    const chosen = unused?.items.filter((item) => unusedSelected.has(item.itemId)) ?? [];
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
    setNotice(`Removed ${itemCount(removed)}.`);
    closeUnused();
    await load();
  };

  const selectedUnused = unusedSelected.size;

  return (
    <div className="space-y-6">
      <section className="agora-hero compact">
        <h2 className="mb-2 text-2xl font-bold">Library</h2>
        <p className="text-muted-foreground">
          Everything you have installed or added, kept with the descriptions saved to disk. This
          list works without the internet.
        </p>
      </section>

      <div className="flex flex-col gap-3 sm:flex-row sm:items-center">
        <label className="relative min-w-48 flex-1">
          <Search
            className="pointer-events-none absolute left-3 top-2.5 h-4 w-4 text-muted-foreground"
            aria-hidden="true"
          />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search your library…"
            aria-label="Search your library"
            className="w-full rounded-lg border border-input bg-background py-2 pl-9 pr-3 text-sm"
          />
        </label>
        <select
          value={game ?? ''}
          onChange={(event) => setGame(event.target.value || null)}
          aria-label="Game"
          className="rounded-lg border border-input bg-background px-3 py-2 text-sm"
        >
          <option value="">All games</option>
          {games.map((id) => (
            <option key={id} value={id}>{gameLabel(id)}</option>
          ))}
        </select>
        <Button variant="outline" onClick={() => void load()}>
          Refresh
        </Button>
      </div>

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

      {unused && unused.items.length > 0 && (
        <div
          className="flex flex-col gap-3 rounded-xl border border-border bg-card p-4 sm:flex-row sm:items-center sm:justify-between"
          data-testid="unused-summary"
        >
          <div>
            <p className="text-sm font-medium">
              Unused: {itemCount(unused.items.length)}, {formatSize(unused.totalSize)}
            </p>
            <p className="text-xs text-muted-foreground">
              No instance uses these. Removing one frees its space; instances keep the files they
              already have.
            </p>
          </div>
          <Button variant="outline" onClick={openUnused}>
            Review unused items
          </Button>
        </div>
      )}

      {state.status === 'loading' ? (
        <div className="rounded-xl border border-dashed border-border bg-card p-6 text-center text-muted-foreground">
          Reading your library…
        </div>
      ) : state.status === 'error' ? (
        <div className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm text-destructive">
          <p>{state.message}</p>
          <button
            type="button"
            onClick={() => void load()}
            className="mt-2 rounded-lg bg-primary px-3 py-1 text-xs font-medium text-primary-foreground hover:bg-primary/90"
          >
            Retry
          </button>
        </div>
      ) : (
        <>
          {state.listing.warnings.length > 0 && (
            <div role="status" className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3 text-sm">
              <p className="font-medium">Some records could not be read.</p>
              <ul className="mt-1 space-y-0.5 text-xs text-muted-foreground">
                {state.listing.warnings.map((warning) => <li key={warning}>{warning}</li>)}
              </ul>
            </div>
          )}

          {state.listing.damaged.length > 0 && (
            <div className="rounded-xl border border-destructive/40 bg-destructive/5 p-3 text-sm">
              <p className="font-medium">Items that could not be read</p>
              <ul className="mt-1 space-y-1 text-xs text-muted-foreground">
                {state.listing.damaged.map((damaged) => (
                  <li key={damaged.itemId}>
                    <span className="font-mono">{damaged.itemId.slice(0, 8)}</span>: {damaged.message}
                  </li>
                ))}
              </ul>
            </div>
          )}

          {items.length === 0 ? (
            <div className="rounded-xl border border-dashed border-border bg-card p-6 text-center text-muted-foreground">
              Nothing in your library yet. Content you install or add is kept here.
            </div>
          ) : visible.length === 0 ? (
            <div className="rounded-xl border border-dashed border-border bg-card p-6 text-center text-muted-foreground">
              No items match this search.
            </div>
          ) : (
            <ul className="space-y-3" data-testid="library-list">
              {visible.map((item) => {
                const itemInstances = instancesForItem(item, instances);
                return (
                  <li
                    key={item.itemId}
                    className="rounded-xl border border-border bg-card p-4"
                    data-testid="library-item"
                  >
                    <div className="flex items-start gap-3">
                      <LibraryIcon item={item} />
                      <div className="min-w-0 flex-1">
                        <div className="flex flex-wrap items-baseline gap-2">
                          <h3 className="text-sm font-semibold">{item.name}</h3>
                          {item.version && (
                            <span className="rounded-full border border-border px-2 py-0.5 text-xs text-muted-foreground">
                              {item.version}
                            </span>
                          )}
                          <span className="text-xs text-muted-foreground">
                            {item.gameId ? gameLabel(item.gameId) : 'Game not recorded'}
                          </span>
                        </div>
                        {item.description ? (
                          <p className="mt-1 text-sm">{item.description}</p>
                        ) : (
                          <p className="mt-1 text-sm italic text-muted-foreground">
                            No description was saved.
                          </p>
                        )}
                        <p className="mt-1 text-xs text-muted-foreground">{item.sourceLabel}</p>
                        <p className="mt-1 text-xs text-muted-foreground">
                          {item.instances.length === 0
                            ? 'Not in an instance.'
                            : `In ${item.instances.map((instance) => instance.name).join(', ')}.`}
                        </p>
                      </div>
                      <div className="shrink-0">
                        <Button
                          variant="outline"
                          size="sm"
                          onClick={() => openAdd(item)}
                          disabled={itemInstances.length === 0}
                          title={
                            itemInstances.length === 0
                              ? 'No instance of this item’s game exists yet.'
                              : undefined
                          }
                        >
                          Add to instance…
                        </Button>
                      </div>
                    </div>
                  </li>
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
              {addTarget
                ? `Choose an instance of ${addTarget.gameId ? gameLabel(addTarget.gameId) : 'the same game'} for ${addTarget.name}.`
                : ''}
            </DialogDescription>
          </DialogHeader>
          {candidates.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              No matching instance yet. Create one first.
            </p>
          ) : (
            <>
              <select
                value={pickedInstance}
                onChange={(event) => setPickedInstance(event.target.value)}
                aria-label="Target instance"
                className="w-full rounded-lg border border-input bg-background px-3 py-2 text-sm"
              >
                <option value="">Choose an instance…</option>
                {candidates.map((instance) => (
                  <option key={instance.instanceId} value={instance.instanceId}>
                    {instance.name}
                  </option>
                ))}
              </select>
              {actionError && (
                <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-2 text-sm">
                  {actionError}
                </p>
              )}
              <div className="flex justify-end gap-2">
                <Button variant="outline" onClick={closeAdd} disabled={busy}>
                  Cancel
                </Button>
                <Button onClick={() => void confirmAdd()} disabled={busy || !pickedInstance}>
                  {busy ? 'Adding…' : 'Add to instance'}
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
              No instance uses these. Removing one deletes it from your library and frees its
              space. Instances keep the files they already have. Nothing is removed until you
              confirm.
            </DialogDescription>
          </DialogHeader>

          {unusedError && (
            <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-2 text-sm">
              {unusedError}
            </p>
          )}

          {unused && unused.warnings.length > 0 && (
            <div role="status" className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-2 text-xs">
              <p className="font-medium">Some instances could not be read, so this list may be incomplete.</p>
              <ul className="mt-1 space-y-0.5 text-muted-foreground">
                {unused.warnings.map((warning) => <li key={warning}>{warning}</li>)}
              </ul>
            </div>
          )}

          <ul className="max-h-80 space-y-2 overflow-auto" data-testid="unused-list">
            {unused?.items.map((item: UnusedItem) => (
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
                    <span className="block text-xs text-muted-foreground">
                      {formatSize(item.totalSize)}
                      {item.gameId ? ` · ${gameLabel(item.gameId)}` : ''}
                    </span>
                  </span>
                </label>
              </li>
            ))}
          </ul>

          {unusedConfirming ? (
            <div className="space-y-3">
              <p className="text-sm">
                Remove {itemCount(selectedUnused)}? This cannot be undone. Instances keep the files
                they already have.
              </p>
              <div className="flex justify-end gap-2">
                <Button variant="outline" onClick={() => setUnusedConfirming(false)} disabled={unusedBusy}>
                  Back
                </Button>
                <Button onClick={() => void confirmRemoveUnused()} disabled={unusedBusy}>
                  {unusedBusy ? 'Removing…' : `Remove ${itemCount(selectedUnused)}`}
                </Button>
              </div>
            </div>
          ) : (
            <div className="flex justify-end gap-2">
              <Button variant="outline" onClick={closeUnused} disabled={unusedBusy}>
                Cancel
              </Button>
              <Button
                onClick={() => setUnusedConfirming(true)}
                disabled={selectedUnused === 0}
              >
                Remove selected…
              </Button>
            </div>
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}
