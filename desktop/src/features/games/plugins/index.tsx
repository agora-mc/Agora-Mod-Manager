/**
 * The Plugins tab of a non-Minecraft game's instance (MASTER_SPEC §26.6): the order the game reads
 * its plugins in, the problems the game's rules find in that order, and the changes a player makes
 * to it. Every change is made by core, and the list shown is the one core returns afterwards.
 */

import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { AlertTriangle, ArrowDown, ArrowUp, CheckCircle2, Download, Lock, RefreshCw, Unlock } from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import {
  checkGamePlugins,
  describeFailure,
  listGamePlugins,
  listLoadOrderLists,
  moveGamePlugin,
  setGamePluginActive,
  setGamePluginLocked,
  sortGamePlugins,
  updateGameMasterlist,
  type PluginChange,
  type PluginFinding,
  type PluginRow,
  type LoadOrderListRow,
  type PluginsView,
  type SortReport,
} from './api';

interface InstancePluginsTabProps {
  gameId: string;
  instanceId: string;
}

type Load =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; view: PluginsView };

export function InstancePluginsTab({ instanceId }: InstancePluginsTabProps) {
  const [load, setLoad] = useState<Load>({ status: 'loading' });
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [sortPreview, setSortPreview] = useState<SortReport | null>(null);
  const [lists, setLists] = useState<LoadOrderListRow[] | null>(null);
  const [listId, setListId] = useState<string | null>(null);
  // Counts the reads, so a reply for an instance the tab no longer shows is dropped.
  const requestId = useRef(0);

  const fetchList = useCallback((id: string | null) => {
    const request = ++requestId.current;
    setLoad({ status: 'loading' });
    setError(null);
    setNotice(null);
    setSortPreview(null);
    listGamePlugins(instanceId, id ?? undefined)
      .then((view) => {
        if (request === requestId.current) setLoad({ status: 'loaded', view });
      })
      .catch((failure: unknown) => {
        if (request === requestId.current) setLoad({ status: 'failed', message: describeFailure(failure) });
      });
  }, [instanceId]);

  useEffect(() => {
    let current = true;
    setLists(null);
    setListId(null);
    listLoadOrderLists(instanceId)
      .then((view) => {
        if (!current) return;
        setLists(view.lists);
        setListId(view.lists[0]?.id ?? null);
      })
      .catch((failure: unknown) => {
        if (!current) return;
        // A game with no list metadata still gets the list's own answer (and its wording).
        setLists([]);
        setError(describeFailure(failure));
      });
    return () => {
      current = false;
      requestId.current++;
    };
  }, [instanceId]);

  useEffect(() => {
    if (lists === null) return;
    fetchList(listId);
  }, [fetchList, listId, lists]);

  const view = load.status === 'loaded' ? load.view : null;

  /** Runs one command. Only one runs at a time; a failure is shown as the backend's own words. */
  const run = async <T,>(label: string, work: () => Promise<T>, done: (value: T) => void) => {
    setBusy(label);
    setError(null);
    setNotice(null);
    try {
      done(await work());
    } catch (failure: unknown) {
      setError(describeFailure(failure));
    } finally {
      setBusy(null);
    }
  };

  const showView = (next: PluginsView) => setLoad({ status: 'loaded', view: next });

  /** A saved change: show the list, or say it was saved and reload it when the re-read failed. */
  const applyChange = (change: PluginChange, saved: string) => {
    if (change.plugins !== null) {
      showView(change.plugins);
      setNotice(saved);
      return;
    }
    fetchList(listId);
    setNotice(`Saved, but the list could not be read back. ${change.reloadError ?? ''}`.trim());
  };

  const toggleActive = (row: PluginRow) =>
    run(
      `active:${row.name}`,
      () => setGamePluginActive(instanceId, row.name, !row.active, listId ?? undefined),
      (change) => {
        const now = row.active ? 'off' : 'on';
        applyChange(change, change.changed ? `${row.name} is now ${now}.` : `${row.name} was already ${now}.`);
      },
    );

  const toggleLock = (row: PluginRow) =>
    run(
      `lock:${row.name}`,
      () => setGamePluginLocked(instanceId, row.name, !row.locked, listId ?? undefined),
      (change) => {
        const now = row.locked ? 'unlocked' : 'locked';
        applyChange(change, change.changed ? `${row.name} is ${now}.` : `${row.name} was already ${now}.`);
      },
    );

  const movePlugin = (row: PluginRow, to: number) =>
    run(
      `move:${row.name}`,
      () => moveGamePlugin(instanceId, row.name, to, listId ?? undefined),
      (change) => {
        showView(change.plugins);
        setNotice(
          change.report.written
            ? `${row.name} moved to position ${change.report.to}.`
            : `${row.name} is already at position ${row.position}.`,
        );
      },
    );

  const checkAgain = () =>
    run('check', () => checkGamePlugins(instanceId, listId ?? undefined), (result) => {
      setLoad((current) =>
        current.status === 'loaded' ? { status: 'loaded', view: { ...current.view, findings: result.findings } } : current,
      );
      setNotice(
        result.status === 'ok'
          ? 'Checked. No problems found.'
          : result.status === 'warnings'
            ? 'Checked. There are things to read, but none of them stops the game from starting.'
            : 'Checked. There are problems that would stop the game from starting.',
      );
    });

  const previewSort = () =>
    run('sort-preview', () => sortGamePlugins(instanceId, true, listId ?? undefined), (change) => setSortPreview(change.report));

  const updateMasterlistOnly = () =>
    run('masterlist', () => updateGameMasterlist(instanceId, listId ?? undefined), (masterlist) => {
      setLoad((current) =>
        current.status === 'loaded' ? { status: 'loaded', view: { ...current.view, masterlist } } : current,
      );
      setNotice('Updated.');
    });

  const applySort = () =>
    run('sort-apply', () => sortGamePlugins(instanceId, false, listId ?? undefined), (change) => {
      showView(change.plugins);
      setSortPreview(null);
      const count = change.report.moves.length;
      setNotice(count === 0 ? 'The list was already in order.' : `Sorted. ${count} ${count === 1 ? 'plugin' : 'plugins'} moved.`);
    });

  if (load.status === 'loading') {
    return <p className="text-sm text-muted-foreground" data-testid="plugins-loading">Reading the plugin list…</p>;
  }

  if (load.status === 'failed' || view === null) {
    return (
      <section className="space-y-3" data-testid="plugins-failed">
        <p role="alert" className="rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
          {load.status === 'failed' ? load.message : 'The plugin list did not load.'}
        </p>
        <Button variant="outline" size="sm" onClick={() => fetchList(listId)}>
          Try again
        </Button>
      </section>
    );
  }

  const hasRows = view.rows.length > 0;
  const showTable = view.supported && view.listExists && hasRows;
  const anyBusy = busy !== null;
  const heading = lists !== null && lists.length > 1 ? 'Load order' : (view.listName || 'Plugins');

  return (
    <section className="space-y-4" aria-labelledby="plugins-heading" data-testid="plugins-tab">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="max-w-2xl space-y-1">
          <h2 id="plugins-heading" className="text-lg font-semibold">{heading}</h2>
          <p className="text-sm text-muted-foreground">
            {view.intro ?? 'The game reads these from the top of this list down. When two change the same thing, the lower one wins.'}
          </p>
          {view.masterlist && (
            <p className="text-xs text-muted-foreground" data-testid="plugins-masterlist">
              Masterlist from {formatDate(view.masterlist.fetchedAt)}
            </p>
          )}
        </div>
        {showTable && (
          <div className="flex flex-wrap gap-2">
            <Button variant="outline" size="sm" onClick={checkAgain} disabled={anyBusy}>
              <RefreshCw />
              Check again
            </Button>
            {view.hasSorterUpdate && (
              <Button variant="outline" size="sm" onClick={updateMasterlistOnly} disabled={anyBusy}>
                <Download />
                {busy === 'masterlist' ? 'Updating…' : (view.sorterUpdateLabel ?? 'Update')}
              </Button>
            )}
            {view.rulesApply && (
              <Button variant="outline" size="sm" onClick={previewSort} disabled={anyBusy}>
                {busy === 'sort-preview' ? 'Sorting…' : 'Sort…'}
              </Button>
            )}
          </div>
        )}
      </header>

      {lists !== null && lists.length > 1 && (
        <div role="tablist" aria-label="Load order lists" className="flex flex-wrap gap-1 rounded-lg bg-muted p-1" data-testid="load-order-tabs">
          {lists.map((list) => {
            const selected = list.id === listId;
            return (
              <button
                key={list.id}
                type="button"
                role="tab"
                aria-selected={selected}
                onClick={() => setListId(list.id)}
                className={[
                  'rounded-md px-3 py-1.5 text-sm font-medium transition-colors',
                  selected ? 'bg-background text-foreground shadow-sm' : 'text-muted-foreground hover:text-foreground',
                ].join(' ')}
              >
                {list.name}
              </button>
            );
          })}
        </div>
      )}

      {error && (
        <p role="alert" className="rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
          {error}
        </p>
      )}
      {notice && (
        <p role="status" className="text-sm text-muted-foreground">
          {notice}
        </p>
      )}

      {view.findings.length > 0 && (
        <div className="space-y-2 rounded-xl border border-border bg-card p-4" data-testid="plugins-findings">
          <h3 className="text-sm font-semibold">Problems with the load order</h3>
          <ul className="space-y-2">
            {view.findings.map((finding, index) => (
              <FindingItem key={index} finding={finding} />
            ))}
          </ul>
        </div>
      )}

      {!view.supported && (
        <EmptyNote>This game does not keep a plugin list, so there is no load order for Agora to manage here.</EmptyNote>
      )}
      {view.supported && !view.listExists && (
        <EmptyNote>
          This instance has no plugin list yet. Agora makes it when the instance's content is deployed, and it
          appears here after that.
        </EmptyNote>
      )}
      {view.supported && view.listExists && !hasRows && (
        <EmptyNote>
          No plugins are in this instance's list yet. Plugins come from the mods you add on the Content tab.
        </EmptyNote>
      )}

      {showTable && (
        <div className="overflow-x-auto rounded-xl border border-border">
          <table className="w-full text-sm">
            <thead className="bg-muted/40 text-left text-xs uppercase tracking-wide text-muted-foreground">
              <tr>
                <th scope="col" className="w-16 px-3 py-2">#</th>
                <th scope="col" className="px-3 py-2">{view.itemHeading ?? 'Name'}</th>
                <th scope="col" className="px-3 py-2 text-right">Change it</th>
              </tr>
            </thead>
            <tbody>
              {view.rows.map((row, index) => (
                <PluginRowView
                  key={row.name}
                  row={row}
                  above={view.rows[index - 1]}
                  below={view.rows[index + 1]}
                  findings={view.findings.filter((finding) => finding.plugins.includes(row.name))}
                  disabled={anyBusy}
                  onToggleActive={() => toggleActive(row)}
                  onToggleLock={() => toggleLock(row)}
                  onMove={(to) => movePlugin(row, to)}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      <Dialog open={sortPreview !== null} onOpenChange={(open) => { if (!open) setSortPreview(null); }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Sort plugins</DialogTitle>
            <DialogDescription>
              Free plugins are put in the order the list's sorter recommends, or below their masters when the sorter
              has nothing saved. Nothing is written until you apply. Locked and always-loaded plugins stay where they
              are.
            </DialogDescription>
          </DialogHeader>
          {sortPreview?.message && (
            <p className="text-sm text-muted-foreground">{sortPreview.message}</p>
          )}
          {sortPreview && sortPreview.moves.length === 0 && (
            <p className="text-sm">The list is already in order. Nothing would change.</p>
          )}
          {sortPreview && sortPreview.moves.length > 0 && (
            <div className="space-y-2">
              <p className="text-sm font-medium">
                {sortPreview.moves.length} {sortPreview.moves.length === 1 ? 'plugin' : 'plugins'} would move:
              </p>
              <ul className="max-h-64 space-y-1 overflow-y-auto text-sm">
                {sortPreview.moves.map((move) => (
                  <li key={`${move.plugin}-${move.from}-${move.to}`} className="font-mono">
                    {move.plugin}: position {move.from} to {move.to}
                  </li>
                ))}
              </ul>
            </div>
          )}
          {sortPreview && sortPreview.blocked.length > 0 && (
            <div className="space-y-1 text-sm">
              <p className="font-medium">These cannot be fixed by sorting:</p>
              <ul className="space-y-1">
                {sortPreview.blocked.map((item) => (
                  <li key={`${item.plugin}-${item.master}`}>
                    {item.plugin} loads above its master {item.master}, and it is locked or always loaded, so it
                    cannot move. Unlock it and move it by hand.
                  </li>
                ))}
              </ul>
            </div>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => setSortPreview(null)} disabled={anyBusy}>
              Cancel
            </Button>
            {sortPreview && sortPreview.moves.length > 0 && (
              <Button onClick={applySort} disabled={anyBusy}>
                {busy === 'sort-apply'
                  ? 'Applying…'
                  : `Apply ${sortPreview.moves.length} ${sortPreview.moves.length === 1 ? 'move' : 'moves'}`}
              </Button>
            )}
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}

function formatDate(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleDateString();
}

function EmptyNote({ children }: { children: ReactNode }) {
  return (
    <p className="rounded-xl border border-dashed border-border bg-card/60 p-4 text-sm text-muted-foreground">{children}</p>
  );
}

function FindingItem({ finding }: { finding: PluginFinding }) {
  return (
    <li className="flex items-start gap-2 text-sm">
      {finding.refusesLaunch ? (
        <AlertTriangle className="mt-0.5 text-destructive" aria-hidden />
      ) : (
        <CheckCircle2 className="mt-0.5 text-muted-foreground" aria-hidden />
      )}
      <div className="space-y-1">
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant={finding.refusesLaunch ? 'destructive' : 'secondary'}>
            {finding.refusesLaunch ? 'Stops launch' : 'Warning'}
          </Badge>
          <span>{finding.message}</span>
        </div>
        {finding.plugins.length > 0 && (
          <p className="text-xs text-muted-foreground">Plugins: {finding.plugins.join(', ')}</p>
        )}
      </div>
    </li>
  );
}

interface PluginRowViewProps {
  row: PluginRow;
  above: PluginRow | undefined;
  below: PluginRow | undefined;
  findings: PluginFinding[];
  disabled: boolean;
  onToggleActive: () => void;
  onToggleLock: () => void;
  onMove: (to: number) => void;
}

function PluginRowView({ row, above, below, findings, disabled, onToggleActive, onToggleLock, onMove }: PluginRowViewProps) {
  // The game's always-loaded plugins sit at the top and cannot be moved or locked.
  const movable = !row.implicit && !row.locked;
  const canMoveUp = movable && above !== undefined && !above.implicit;
  const canMoveDown = movable && below !== undefined && !below.implicit;
  const upReason = row.implicit
    ? 'Always loaded, so it cannot be moved.'
    : row.locked
      ? 'Unlock it to move it.'
      : 'It cannot go any higher.';
  const downReason = row.implicit
    ? 'Always loaded, so it cannot be moved.'
    : row.locked
      ? 'Unlock it to move it.'
      : 'It is already last.';

  return (
    <tr className="border-t border-border align-top" data-testid="plugin-row" data-plugin={row.name}>
      <td className="px-3 py-2 tabular-nums text-muted-foreground">{row.position}</td>
      <td className="px-3 py-2">
        <div className="flex flex-wrap items-center gap-2">
          <span className={row.active ? 'font-mono' : 'font-mono text-muted-foreground line-through'}>{row.name}</span>
          {row.implicit && <Badge variant="secondary">Always loaded</Badge>}
          {row.locked && <Badge variant="outline">Place locked</Badge>}
          {row.managed && <Badge variant="outline">Added by a content layer</Badge>}
          {!row.present && <Badge variant="destructive">File missing</Badge>}
          {row.badges.map((badge) => (
            <Badge key={badge} variant="outline">{badge}</Badge>
          ))}
        </div>
        {row.masters.length > 0 && (
          <p className="mt-1 text-xs text-muted-foreground">Needs {row.masters.join(', ')}</p>
        )}
        {findings.map((finding, index) => (
          <p key={index} className={finding.refusesLaunch ? 'mt-1 text-xs text-destructive' : 'mt-1 text-xs text-muted-foreground'}>
            {finding.message}
          </p>
        ))}
      </td>
      <td className="px-3 py-2">
        <div className="flex flex-wrap justify-end gap-1">
          <Button
            variant="outline"
            size="sm"
            role="switch"
            aria-checked={row.active}
            aria-label={`${row.name} enabled`}
            title={row.implicit ? 'Always loaded, so it cannot be turned off here.' : undefined}
            disabled={disabled || row.implicit}
            onClick={onToggleActive}
          >
            {row.active ? 'On' : 'Off'}
          </Button>
          <Button
            variant="outline"
            size="sm"
            aria-pressed={row.locked}
            aria-label={`${row.locked ? 'Unlock' : 'Lock'} ${row.name}`}
            title={row.implicit ? 'Always loaded, so its place cannot be locked.' : 'Keep this plugin where it is when sorting.'}
            disabled={disabled || row.implicit}
            onClick={onToggleLock}
          >
            {row.locked ? <Unlock /> : <Lock />}
            {row.locked ? 'Unlock' : 'Lock'}
          </Button>
          <Button
            variant="outline"
            size="sm"
            aria-label={`Move ${row.name} up`}
            title={canMoveUp ? 'Move up one place' : upReason}
            disabled={disabled || !canMoveUp}
            onClick={() => onMove(row.position - 1)}
          >
            <ArrowUp />
          </Button>
          <Button
            variant="outline"
            size="sm"
            aria-label={`Move ${row.name} down`}
            title={canMoveDown ? 'Move down one place' : downReason}
            disabled={disabled || !canMoveDown}
            onClick={() => onMove(row.position + 1)}
          >
            <ArrowDown />
          </Button>
        </div>
      </td>
    </tr>
  );
}
