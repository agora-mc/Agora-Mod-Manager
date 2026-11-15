/**
 * The Plugins tab of a non-Minecraft game's instance (MASTER_SPEC §26.6): the order the game reads
 * its plugins in, the problems the game's rules find in that order, and the changes a player makes
 * to it. Every change is made by core, and the list shown is the one core returns afterwards.
 */

import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Download,
  Lock,
  RefreshCw,
  Unlock,
} from 'lucide-react';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { formatError, getSetting, setSetting } from '@/lib/tauri';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { isGamesError } from '../shared/types';
import {
  applyConflictAction,
  checkGamePlugins,
  describeFailure,
  listGamePlugins,
  listLoadOrderLists,
  listLoadOrderRules,
  moveGamePlugin,
  refreshOverlaps,
  refreshSorterNotes,
  removeLoadOrderRule,
  setGamePluginActive,
  setGamePluginLocked,
  sortGamePlugins,
  updateGameMasterlist,
  type ConflictPair,
  type OverlapsView,
  type PluginChange,
  type PluginFinding,
  type PluginRow,
  type RuleChange,
  type RuleView,
  type LoadOrderListRow,
  type PluginsView,
  type SortReport,
} from './api';

interface InstancePluginsTabProps {
  gameId: string;
  instanceId: string;
}

const NETWORK_PLUGINS_ENABLED = 'network_plugins_enabled';
const LOCKDOWN_MODE_ENABLED = 'network_lockdown_enabled';

type Load =
  | { status: 'loading' }
  | { status: 'failed'; message: string }
  | { status: 'loaded'; view: PluginsView };

/** What the tab learned about whether plugin code may reach the network. */
interface NetworkAccess {
  /** `unknown` when the setting could not be read; the update is then left enabled. */
  network: 'on' | 'off' | 'unknown';
  lockdown: 'on' | 'off';
}

/** What an action on a conflict left behind, with its way back. */
interface ConflictNotice {
  text: string;
  undo: () => Promise<void>;
}

/** A move that a load order rule refuses, waiting for the player to allow it. */
interface MoveConfirm {
  row: PluginRow;
  to: number;
  item: string;
  after: string;
}

export function InstancePluginsTab({ instanceId }: InstancePluginsTabProps) {
  const [load, setLoad] = useState<Load>({ status: 'loading' });
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [sortPreview, setSortPreview] = useState<SortReport | null>(null);
  const [lists, setLists] = useState<LoadOrderListRow[] | null>(null);
  const [listId, setListId] = useState<string | null>(null);
  const [networkAccess, setNetworkAccess] = useState<NetworkAccess | null>(null);
  const [rules, setRules] = useState<RuleView[] | null>(null);
  const [rulesError, setRulesError] = useState<string | null>(null);
  const [rulesOpen, setRulesOpen] = useState(false);
  const [overlapsError, setOverlapsError] = useState<string | null>(null);
  const [conflictFilter, setConflictFilter] = useState<string | null>(null);
  const [expandedPair, setExpandedPair] = useState<string | null>(null);
  const [conflictNotice, setConflictNotice] = useState<ConflictNotice | null>(null);
  const [moveConfirm, setMoveConfirm] = useState<MoveConfirm | null>(null);
  // Counts the reads, so a reply for an instance the tab no longer shows is dropped.
  const requestId = useRef(0);
  // Counts the background notes fetches, so a result for a view that has since changed is dropped.
  const notesRequestId = useRef(0);
  // The same for conflicts and rules, fetched without blocking the list.
  const overlapsRequestId = useRef(0);
  const rulesRequestId = useRef(0);
  const conflictsCardRef = useRef<HTMLDivElement | null>(null);

  /** Fetch the sorter's notes without blocking the list; a newer read or change wins. */
  const loadSorterNotes = useCallback((id: string | null) => {
    const ticket = ++notesRequestId.current;
    refreshSorterNotes(instanceId, id ?? undefined)
      .then((result) => {
        if (ticket !== notesRequestId.current) return;
        setLoad((current) =>
          current.status === 'loaded'
            ? { status: 'loaded', view: { ...current.view, notes: result.notes, notesOutOfDate: result.notesOutOfDate } }
            : current,
        );
      })
      .catch(() => {
        // The list is already shown; a failed notes fetch is in the plugin log.
      });
  }, [instanceId]);

  /** Ask the reader for this list's conflicts without blocking the list; a newer read wins. */
  const loadConflicts = useCallback((id: string | null) => {
    const ticket = ++overlapsRequestId.current;
    setOverlapsError(null);
    refreshOverlaps(instanceId, id ?? undefined)
      .then((report) => {
        if (ticket !== overlapsRequestId.current) return;
        setLoad((current) =>
          current.status === 'loaded'
            ? { status: 'loaded', view: { ...current.view, overlaps: report, overlapsOutOfDate: false } }
            : current,
        );
      })
      .catch((failure: unknown) => {
        if (ticket !== overlapsRequestId.current) return;
        // A reader that fails is "could not check", never "no conflicts".
        setOverlapsError(describeFailure(failure));
        setLoad((current) =>
          current.status === 'loaded'
            ? { status: 'loaded', view: { ...current.view, overlaps: null, overlapsOutOfDate: false } }
            : current,
        );
      });
  }, [instanceId]);

  /** Read the load order rules; the count in the section heading comes from here. */
  const loadRules = useCallback((id: string | null) => {
    const ticket = ++rulesRequestId.current;
    setRulesError(null);
    listLoadOrderRules(instanceId, id ?? undefined)
      .then((next) => {
        if (ticket === rulesRequestId.current) setRules(next);
      })
      .catch((failure: unknown) => {
        if (ticket !== rulesRequestId.current) return;
        setRules(null);
        setRulesError(describeFailure(failure));
      });
  }, [instanceId]);

  const fetchList = useCallback((id: string | null) => {
    const request = ++requestId.current;
    notesRequestId.current++;
    overlapsRequestId.current++;
    rulesRequestId.current++;
    setLoad({ status: 'loading' });
    setError(null);
    setNotice(null);
    setSortPreview(null);
    setOverlapsError(null);
    setRules(null);
    setRulesError(null);
    setConflictFilter(null);
    setConflictNotice(null);
    setExpandedPair(null);
    listGamePlugins(instanceId, id ?? undefined)
      .then((view) => {
        if (request !== requestId.current) return;
        setLoad({ status: 'loaded', view });
        if (view.notesOutOfDate) loadSorterNotes(id);
        if (view.hasOverlapsReader && (view.overlapsOutOfDate || view.overlaps === null)) loadConflicts(id);
      })
      .catch((failure: unknown) => {
        if (request === requestId.current) setLoad({ status: 'failed', message: describeFailure(failure) });
      });
  }, [instanceId, loadConflicts, loadSorterNotes]);

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
      notesRequestId.current++;
      overlapsRequestId.current++;
      rulesRequestId.current++;
    };
  }, [instanceId]);

  useEffect(() => {
    if (lists === null) return;
    fetchList(listId);
  }, [fetchList, listId, lists]);

  useEffect(() => {
    if (lists === null) return;
    loadRules(listId);
  }, [listId, lists, loadRules]);

  const view = load.status === 'loaded' ? load.view : null;
  const allFindings = view ? [...view.findings, ...view.notes] : [];

  // A sorter update runs plugin code, so the tab reads the same two settings
  // Settings > Plugins and Settings > Privacy use. A failed read leaves the
  // update enabled: it reports its own failure, and guessing "off" would hide
  // a working update behind a switch we could not read.
  const hasSorterUpdate = view?.hasSorterUpdate === true;
  useEffect(() => {
    if (!hasSorterUpdate) {
      setNetworkAccess(null);
      return;
    }
    let current = true;
    Promise.allSettled([getSetting(NETWORK_PLUGINS_ENABLED), getSetting(LOCKDOWN_MODE_ENABLED)]).then(
      ([network, lockdown]) => {
        if (!current) return;
        setNetworkAccess({
          network: network.status === 'fulfilled' ? (network.value === true ? 'on' : 'off') : 'unknown',
          lockdown: lockdown.status === 'fulfilled' && lockdown.value === true ? 'on' : 'off',
        });
      },
    );
    return () => {
      current = false;
    };
  }, [hasSorterUpdate, instanceId]);

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

  const showView = (next: PluginsView) => {
    notesRequestId.current++;
    overlapsRequestId.current++;
    setLoad({ status: 'loaded', view: next });
    if (next.notesOutOfDate) loadSorterNotes(listId);
    if (next.hasOverlapsReader && (next.overlapsOutOfDate || next.overlaps === null)) loadConflicts(listId);
  };

  /** Show what a saved rule change left behind, including a reader that could not be run again. */
  const applyRuleChange = (change: RuleChange, message: string) => {
    setRules(change.rules);
    if (change.overlaps !== null) {
      overlapsRequestId.current++;
      setOverlapsError(null);
      setLoad((current) =>
        current.status === 'loaded'
          ? { status: 'loaded', view: { ...current.view, overlaps: change.overlaps, overlapsOutOfDate: false } }
          : current,
      );
    } else {
      setOverlapsError(change.overlapsError);
    }
    setNotice(message);
  };

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

  /** Move a plugin, or ask first when a load order rule is in the way. */
  const moveTo = async (row: PluginRow, to: number, breakRule: boolean) => {
    setBusy(`move:${row.name}`);
    setError(null);
    setNotice(null);
    setConflictNotice(null);
    try {
      const change = await moveGamePlugin(instanceId, row.name, to, listId ?? undefined, breakRule);
      showView(change.plugins);
      setNotice(
        change.report.written
          ? `${row.name} moved to position ${change.report.to}.`
          : `${row.name} is already at position ${row.position}.`,
      );
    } catch (failure: unknown) {
      if (isGamesError(failure) && failure.code === 'BREAKS_RULE') {
        const details = failure.details as { item?: unknown; after?: unknown } | undefined;
        if (typeof details?.item === 'string' && typeof details?.after === 'string') {
          setMoveConfirm({ row, to, item: details.item, after: details.after });
          return;
        }
      }
      setError(describeFailure(failure));
    } finally {
      setBusy(null);
    }
  };

  const movePlugin = (row: PluginRow, to: number) => {
    void moveTo(row, to, false);
  };

  const conflictKey = (pair: ConflictPair) => `${pair.winner}\u0000${pair.loser}`;

  /** Let the loser win, or keep this order, and remember how to put it back. */
  const runConflictAction = async (pair: ConflictPair, letLoserWin: boolean) => {
    const previous = view?.rows.find((row) => row.name.toLowerCase() === pair.loser.toLowerCase())?.position;
    setBusy(`conflict:${conflictKey(pair)}`);
    setError(null);
    setNotice(null);
    setConflictNotice(null);
    try {
      const change = await applyConflictAction(
        instanceId,
        pair.winner,
        pair.loser,
        letLoserWin,
        listId ?? undefined,
      );
      notesRequestId.current++;
      overlapsRequestId.current++;
      setLoad((current) =>
        current.status === 'loaded'
          ? {
              status: 'loaded',
              view: { ...change.plugins, overlaps: change.overlaps, overlapsOutOfDate: false },
            }
          : current,
      );
      setRules(change.rules);
      setOverlapsError(change.overlapsError);
      const item = letLoserWin ? pair.loser : pair.winner;
      const after = letLoserWin ? pair.winner : pair.loser;
      const undo = async () => {
        setBusy('conflict-undo');
        setError(null);
        setNotice(null);
        try {
          const removed = await removeLoadOrderRule(instanceId, item, after, listId ?? undefined);
          setRules(removed.rules);
          let next: PluginsView | null = null;
          if (letLoserWin && previous !== undefined) {
            const moved = await moveGamePlugin(instanceId, pair.loser, previous, listId ?? undefined);
            next = moved.plugins;
          }
          if (next !== null) {
            setLoad({
              status: 'loaded',
              view: { ...next, overlaps: removed.overlaps, overlapsOutOfDate: false },
            });
          } else {
            setLoad((current) =>
              current.status === 'loaded'
                ? {
                    status: 'loaded',
                    view: { ...current.view, overlaps: removed.overlaps, overlapsOutOfDate: false },
                  }
                : current,
            );
          }
          setOverlapsError(removed.overlapsError);
          setConflictNotice(null);
          setNotice(
            letLoserWin
              ? `Put ${pair.loser} back and removed the rule.`
              : `Removed the rule that ${pair.winner} loads after ${pair.loser}.`,
          );
          // The move back has changed the order, so read the conflicts that order now has.
          if (letLoserWin) loadConflicts(listId);
        } catch (failure: unknown) {
          setError(describeFailure(failure));
        } finally {
          setBusy(null);
        }
      };
      setConflictNotice({
        text: letLoserWin
          ? `Let ${pair.loser} win. It now loads after ${pair.winner}.`
          : `Kept this order: ${pair.loser} loads before ${pair.winner}.`,
        undo,
      });
    } catch (failure: unknown) {
      setError(describeFailure(failure));
    } finally {
      setBusy(null);
    }
  };

  const removeRule = (rule: RuleView) =>
    run(
      `rule:${rule.item}:${rule.after}`,
      () => removeLoadOrderRule(instanceId, rule.item, rule.after, listId ?? undefined),
      (change) => applyRuleChange(change, `Removed the rule that ${rule.item} loads after ${rule.after}.`),
    );

  /** Badge click: narrow the conflicts card to this plugin and bring it into view. */
  const showConflictsFor = (name: string) => {
    setConflictFilter(name);
    setExpandedPair(null);
    conflictsCardRef.current?.scrollIntoView?.({ block: 'start' });
  };

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

  // The same setting Settings > Plugins writes; nothing else changes if it fails.
  const turnOnPluginNetwork = async () => {
    setBusy('network');
    setError(null);
    setNotice(null);
    try {
      await setSetting(NETWORK_PLUGINS_ENABLED, true);
      setNetworkAccess((current) => (current === null ? current : { ...current, network: 'on' }));
      setNotice('Network for plugins is on. You can turn it off in Settings > Plugins.');
    } catch (failure: unknown) {
      setError(formatError(failure));
    } finally {
      setBusy(null);
    }
  };

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
  const sorterUpdateLabel = view.sorterUpdateLabel ?? 'Update';
  const lockdownOn = networkAccess?.lockdown === 'on';
  const networkOff = networkAccess?.network === 'off';
  const updateBlocked = lockdownOn || networkOff;
  const showNetworkGate = showTable && view.hasSorterUpdate && networkAccess !== null && updateBlocked;
  const findingsTitle = allFindings.some((finding) => finding.level !== 'info')
    ? 'Problems with the load order'
    : 'Notes on the load order';
  const ruleCount = rules?.length ?? 0;
  const showConflicts = view.hasOverlapsReader;
  const overlaps = view.overlaps;

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
              <Button variant="outline" size="sm" onClick={updateMasterlistOnly} disabled={anyBusy || updateBlocked}>
                <Download />
                {busy === 'masterlist' ? 'Updating…' : sorterUpdateLabel}
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

      {showNetworkGate && (
        <div className="space-y-2" data-testid="plugins-network-gate">
          {lockdownOn ? (
            <p className="text-sm text-muted-foreground">
              Lockdown Mode is on, so plugins can&apos;t reach the network. Turn it off in Settings &gt; Privacy to use {sorterUpdateLabel}.
            </p>
          ) : (
            <>
              <p className="text-sm text-muted-foreground">
                {sorterUpdateLabel} needs plugins to reach the network, which is turned off.
              </p>
              <Button variant="outline" size="sm" onClick={() => void turnOnPluginNetwork()} disabled={anyBusy}>
                {busy === 'network' ? 'Turning on…' : 'Turn on network for plugins'}
              </Button>
            </>
          )}
        </div>
      )}

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

      {allFindings.length > 0 && (
        <div className="space-y-2 rounded-xl border border-border bg-card p-4" data-testid="plugins-findings">
          <h3 className="text-sm font-semibold">{findingsTitle}</h3>
          <ul className="space-y-2">
            {allFindings.map((finding, index) => (
              <FindingItem key={index} finding={finding} />
            ))}
          </ul>
        </div>
      )}

      {showConflicts && (
        <div
          ref={conflictsCardRef}
          className="space-y-3 rounded-xl border border-border bg-card p-4"
          data-testid="plugins-conflicts"
          data-state={
            overlapsError !== null
              ? 'failed'
              : overlaps === null
                ? 'reading'
                : overlaps.pairs.length > 0
                  ? 'pairs'
                  : 'none'
          }
        >
          <div className="flex flex-wrap items-start justify-between gap-2">
            <h3 className="text-sm font-semibold">Plugins that override each other</h3>
            {conflictFilter !== null && (
              <button
                type="button"
                onClick={() => setConflictFilter(null)}
                className="rounded-full border border-border px-2 py-0.5 text-xs text-muted-foreground hover:text-foreground"
              >
                Showing {conflictFilter} · Show all
              </button>
            )}
          </div>

          {overlapsError !== null ? (
            <p className="text-sm text-muted-foreground" data-testid="plugins-conflicts-error">
              Could not check: {overlapsError}
            </p>
          ) : overlaps === null ? (
            <p className="text-sm text-muted-foreground">
              Reading your plugins&apos; records. The first time takes a few seconds.
            </p>
          ) : (
            <ConflictsReport
              report={overlaps}
              filter={conflictFilter}
              expandedPair={expandedPair}
              onTogglePair={(pair) =>
                setExpandedPair((current) => (current === conflictKey(pair) ? null : conflictKey(pair)))
              }
              disabled={anyBusy}
              onKeepOrder={(pair) => void runConflictAction(pair, false)}
              onLetLose={(pair) => void runConflictAction(pair, true)}
            />
          )}

          {conflictNotice !== null && (
            <div className="flex flex-wrap items-center gap-2 text-sm" data-testid="plugins-conflict-notice">
              <span className="text-muted-foreground">{conflictNotice.text}</span>
              <Button
                variant="outline"
                size="sm"
                onClick={() => void conflictNotice.undo()}
                disabled={anyBusy}
              >
                Undo
              </Button>
            </div>
          )}
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
                  findings={allFindings.filter((finding) => finding.plugins.includes(row.name))}
                  conflictCounts={conflictCounts(overlaps, row.name)}
                  disabled={anyBusy}
                  onToggleActive={() => toggleActive(row)}
                  onToggleLock={() => toggleLock(row)}
                  onMove={(to) => movePlugin(row, to)}
                  onShowConflicts={() => showConflictsFor(row.name)}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {rulesError !== null && (
        <p role="alert" className="text-sm text-muted-foreground" data-testid="plugins-rules-error">
          Your load order rules could not be read: {rulesError}
        </p>
      )}

      {rules !== null && rules.length > 0 && (
        <section className="rounded-xl border border-border bg-card p-4" data-testid="plugins-rules">
          <button
            type="button"
            aria-expanded={rulesOpen}
            onClick={() => setRulesOpen((open) => !open)}
            className="flex w-full items-center gap-2 text-left text-sm font-semibold"
          >
            {rulesOpen ? <ChevronDown aria-hidden /> : <ChevronRight aria-hidden />}
            Your load order rules ({ruleCount})
          </button>
          {rulesOpen && (
            <ul className="mt-3 space-y-2">
              {rules.map((rule) => (
                <li key={`${rule.item}\u0000${rule.after}`} className="flex flex-wrap items-start justify-between gap-2 text-sm">
                  <div>
                    <span>
                      {rule.item} loads after {rule.after}
                    </span>
                    {rule.unused !== null && (
                      <p className="text-xs text-muted-foreground" data-testid="rule-unused">
                        Unused: {rule.unused} is not in the list right now.
                      </p>
                    )}
                  </div>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => void removeRule(rule)}
                    disabled={anyBusy}
                  >
                    Remove
                  </Button>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}

      <Dialog open={moveConfirm !== null} onOpenChange={(open) => { if (!open) setMoveConfirm(null); }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Move {moveConfirm?.row.name}</DialogTitle>
            <DialogDescription>
              This breaks your rule &apos;{moveConfirm?.item} loads after {moveConfirm?.after}&apos;.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setMoveConfirm(null)} disabled={anyBusy}>
              Cancel
            </Button>
            <Button
              onClick={() => {
                const waiting = moveConfirm;
                setMoveConfirm(null);
                if (waiting !== null) void moveTo(waiting.row, waiting.to, true);
              }}
              disabled={anyBusy}
            >
              Move and remove the rule
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={sortPreview !== null} onOpenChange={(open) => { if (!open) setSortPreview(null); }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Sort plugins</DialogTitle>
            <DialogDescription>
              Free plugins are put in the order the list's sorter recommends, or below their masters when the sorter
              has nothing saved. Nothing is written until you apply. Locked and always-loaded plugins stay where they
              are.
              {ruleCount > 0 && ` Your ${ruleCount} load order ${ruleCount === 1 ? 'rule is' : 'rules are'} kept.`}
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

const pairKey = (pair: ConflictPair) => `${pair.winner}\u0000${pair.loser}`;

/** How many record changes this plugin wins and loses, from the pairs already classified. */
function conflictCounts(report: OverlapsView | null, name: string): { wins: number; loses: number } {
  if (report === null) return { wins: 0, loses: 0 };
  const wanted = name.toLowerCase();
  let wins = 0;
  let loses = 0;
  for (const pair of report.pairs) {
    if (pair.winner.toLowerCase() === wanted) wins += pair.count;
    if (pair.loser.toLowerCase() === wanted) loses += pair.count;
  }
  return { wins, loses };
}

interface ConflictsReportProps {
  report: OverlapsView;
  filter: string | null;
  expandedPair: string | null;
  onTogglePair: (pair: ConflictPair) => void;
  disabled: boolean;
  onKeepOrder: (pair: ConflictPair) => void;
  onLetLose: (pair: ConflictPair) => void;
}

function ConflictsReport({
  report,
  filter,
  expandedPair,
  onTogglePair,
  disabled,
  onKeepOrder,
  onLetLose,
}: ConflictsReportProps) {
  const wanted = filter?.toLowerCase() ?? null;
  const pairs = report.pairs
    .filter(
      (pair) =>
        wanted === null || pair.winner.toLowerCase() === wanted || pair.loser.toLowerCase() === wanted,
    )
    .sort(
      (a, b) =>
        Number(b.newSinceLastLaunch) - Number(a.newSinceLastLaunch)
        || b.count - a.count
        || a.winner.localeCompare(b.winner)
        || a.loser.localeCompare(b.loser),
    );
  const footer: string[] = [];
  if (report.patched > 0) {
    footer.push(`${report.patched} ${report.patched === 1 ? 'pair is' : 'pairs are'} handled by a patch.`);
  }
  if (report.implicitPairs > 0) {
    footer.push(
      `${report.implicitPairs} ${report.implicitPairs === 1 ? 'is' : 'are'} between the game's own files, which can't be reordered.`,
    );
  }
  if (report.decided > 0) {
    footer.push(`${report.decided} decided by you.`);
  }

  if (pairs.length === 0) {
    if (filter !== null) {
      return <p className="text-sm text-muted-foreground">{filter} wins and loses no record changes.</p>;
    }
    const resolved: string[] = [];
    if (report.patched > 0) {
      resolved.push(`${report.patched} ${report.patched === 1 ? 'overlap is' : 'overlaps are'} handled by a patch.`);
    }
    if (report.implicitPairs > 0) {
      resolved.push(`${report.implicitPairs} ${report.implicitPairs === 1 ? 'is' : 'are'} between the game's own files.`);
    }
    if (report.decided > 0) {
      resolved.push(`${report.decided} ${report.decided === 1 ? 'is' : 'are'} decided by you.`);
    }
    return (
      <div className="space-y-1 text-sm">
        <p>No conflicts.{resolved.length > 0 ? ` ${resolved.join(' ')}` : ''}</p>
        {report.notes.map((note, index) => (
          <p key={index} className="text-xs text-muted-foreground">
            {note.plugins.length > 0 ? `${note.plugins.join(', ')}: ` : ''}
            {note.message}
          </p>
        ))}
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <ul className="divide-y divide-border">
        {pairs.map((pair) => {
          const open = expandedPair === pairKey(pair);
          const more = pair.count - pair.labels.length;
          return (
            <li key={pairKey(pair)} className="py-2 first:pt-0 last:pb-0" data-testid="conflict-pair">
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div className="flex flex-wrap items-center gap-2 text-sm">
                  <button
                    type="button"
                    aria-expanded={open}
                    aria-label={`${open ? 'Hide' : 'Show'} the records where ${pair.winner} wins over ${pair.loser}`}
                    onClick={() => onTogglePair(pair)}
                    className="text-muted-foreground hover:text-foreground"
                  >
                    {open ? <ChevronDown aria-hidden /> : <ChevronRight aria-hidden />}
                  </button>
                  {pair.newSinceLastLaunch && <Badge variant="secondary">New</Badge>}
                  <span>
                    <strong>{pair.winner}</strong> wins over <strong>{pair.loser}</strong> ·{' '}
                    {pair.count} {pair.count === 1 ? 'record' : 'records'}
                  </span>
                </div>
                <div className="flex flex-wrap gap-1">
                  <Button variant="secondary" size="sm" onClick={() => onKeepOrder(pair)} disabled={disabled}>
                    Keep this order
                  </Button>
                  <Button variant="outline" size="sm" onClick={() => onLetLose(pair)} disabled={disabled}>
                    Let {pair.loser} win
                  </Button>
                </div>
              </div>
              {open && (
                <div className="mt-2 space-y-1 pl-7 text-xs text-muted-foreground">
                  <ul className="space-y-0.5">
                    {pair.labels.map((label) => (
                      <li key={label}>{label}</li>
                    ))}
                  </ul>
                  {more > 0 && <p>and {more} more</p>}
                </div>
              )}
            </li>
          );
        })}
      </ul>
      {footer.length > 0 && <p className="text-xs text-muted-foreground">{footer.join(' ')}</p>}
      {report.notes.map((note, index) => (
        <p key={index} className="text-xs text-muted-foreground">
          {note.plugins.length > 0 ? `${note.plugins.join(', ')}: ` : ''}
          {note.message}
        </p>
      ))}
    </div>
  );
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
          <Badge
            variant={
              finding.refusesLaunch ? 'destructive' : finding.level === 'info' ? 'outline' : 'secondary'
            }
          >
            {finding.refusesLaunch ? 'Stops launch' : finding.level === 'info' ? 'Info' : 'Warning'}
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
  conflictCounts: { wins: number; loses: number };
  disabled: boolean;
  onToggleActive: () => void;
  onToggleLock: () => void;
  onMove: (to: number) => void;
  onShowConflicts: () => void;
}

function PluginRowView({
  row,
  above,
  below,
  findings,
  conflictCounts,
  disabled,
  onToggleActive,
  onToggleLock,
  onMove,
  onShowConflicts,
}: PluginRowViewProps) {
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
          {(conflictCounts.wins > 0 || conflictCounts.loses > 0) && (
            <button
              type="button"
              data-testid="plugin-conflict-badge"
              onClick={onShowConflicts}
              className="text-xs text-muted-foreground hover:text-foreground hover:underline"
              title="See the record changes this plugin wins and loses"
            >
              {conflictCounts.wins > 0 && `Wins ${conflictCounts.wins}`}
              {conflictCounts.wins > 0 && conflictCounts.loses > 0 && ' · '}
              {conflictCounts.loses > 0 && `Loses ${conflictCounts.loses}`}
            </button>
          )}
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
