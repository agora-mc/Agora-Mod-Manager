/**
 * An instance's Content tab: the content it has, in load order, with where each item came from.
 * The user turns items on and off, moves them, removes them, and adds items from the library.
 * An archive with a FOMOD installer opens the installer's questions before anything is added.
 *
 * The list itself is Minecraft's installed-content panel, reused through the adapter in
 * `panelAdapter.ts`. Reusing it means search, sort, filters and bulk actions stay in one place;
 * the panel only shows the columns and filters the game's data can fill, and its optional
 * load-order mode adds positions, Move up/down and drag-and-drop.
 */

import { useCallback, useEffect, useMemo, useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { DropdownMenuItem } from '@/components/ui/dropdown-menu';
import { useConfirm } from '@/components/ui/confirm';
import { InstalledContentPanel } from '@/components/installed-content/InstalledContentPanel';
import type { LoadOrderConfig } from '@/components/installed-content/types';
import type { InstalledContentRow } from '@/lib/tauri';
import { itemsForInstanceOf, listLibrary, type LibraryItem } from '@/features/library/api';
import { openGameInstanceFolder } from '../instances/api';
import { listGameTools, runGameTool, saveGameToolInputs, type ToolRow } from '../tools/api';
import { ToolInputsForm } from '../tools/ToolInputsForm';
import { FomodWizard } from '../fomod';
import { isGamesError } from '../shared/types';
import {
  addInstanceContent,
  listInstanceContent,
  moveInstanceContent,
  removeInstanceContent,
  setInstanceContentEnabled,
  type InstanceContentList,
  type InstanceContentRow,
  type InstanceAddOutcome,
} from './api';
import {
  GAME_CONTENT_COLUMNS,
  GAME_CONTENT_COLUMN_LABELS,
  GAME_CONTENT_FILTERS,
  toPanelRows,
  whereItGoes,
} from './panelAdapter';

export interface InstanceContentTabProps {
  /** The game the instance belongs to. The instance's own record says which game it is. */
  gameId: string;
  instanceId: string;
}

function errorMessage(error: unknown): string {
  if (isGamesError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return 'Something went wrong. Try again.';
}

function addedNotice(outcome: Extract<InstanceAddOutcome, { status: 'added' }>, name: string): string {
  if (outcome.thunderstore) {
    const pkg = outcome.thunderstore;
    const missing = pkg.missingDependencies.length > 0
      ? ` It needs ${pkg.missingDependencies.join(', ')}, which this instance does not have yet.`
      : '';
    return `Installed Thunderstore package ${pkg.packageId} ${pkg.version}. ${pkg.summary}${missing}`;
  }
  return outcome.placementReason
    ? `Added ${name}. ${outcome.placementReason}`
    : `Added ${name}.`;
}

export function InstanceContentTab({ gameId, instanceId }: InstanceContentTabProps) {
  const { confirm } = useConfirm();
  const [content, setContent] = useState<InstanceContentList | null>(null);
  const [contentError, setContentError] = useState<string | null>(null);
  const [store, setStore] = useState<LibraryItem[] | null>(null);
  const [storeError, setStoreError] = useState<string | null>(null);
  const [pickedId, setPickedId] = useState('');
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [wizardFor, setWizardFor] = useState<string | null>(null);
  const [itemTools, setItemTools] = useState<ToolRow[]>([]);
  const [itemToolsError, setItemToolsError] = useState<string | null>(null);
  const [toolTarget, setToolTarget] = useState<{ tool: ToolRow; item: InstanceContentRow } | null>(null);
  const [toolBusy, setToolBusy] = useState(false);
  const [toolError, setToolError] = useState<string | null>(null);
  const [placementNote, setPlacementNote] = useState<{ topLevel: string[]; note: string } | null>(null);

  const refresh = useCallback(async () => {
    setContentError(null);
    try {
      setContent(await listInstanceContent(instanceId));
    } catch (error) {
      setContent(null);
      setContentError(errorMessage(error));
    }
  }, [instanceId]);

  // The Library's own listing, because it alone knows every game an item is tied to: it counts the
  // instances that use an item, which is what ties most Minecraft mods to Minecraft. Moving or
  // switching a row does not change the library, so this is read only when it can have changed.
  const refreshLibrary = useCallback(async () => {
    setStoreError(null);
    try {
      setStore((await listLibrary()).items);
    } catch (error) {
      setStore(null);
      setStoreError(errorMessage(error));
    }
  }, []);

  useEffect(() => {
    setNotice(null);
    setActionError(null);
    setPickedId('');
    setItemToolsError(null);
    void refresh();
    void refreshLibrary();
    listGameTools(instanceId)
      .then((view) => setItemTools(view.tools.filter((tool) => tool.scope === 'item')))
      .catch((error: unknown) => {
        setItemTools([]);
        setItemToolsError(errorMessage(error));
      });
  }, [instanceId, refresh, refreshLibrary]);

  const items = content?.items ?? [];
  const panelRows = useMemo(() => toPanelRows(items), [items]);
  const originalsByKey = useMemo(
    () => new Map(items.map((row) => [row.layerId, row])),
    [items],
  );

  const originalFor = (row: InstalledContentRow): InstanceContentRow => {
    const original = originalsByKey.get(row.key);
    if (!original) throw new Error('That item is no longer in this instance.');
    return original;
  };

  const originalsFor = (rows: InstalledContentRow[]): InstanceContentRow[] =>
    rows.flatMap((row) => {
      const original = originalsByKey.get(row.key);
      return original ? [original] : [];
    });

  const loadOrder = useMemo<LoadOrderConfig>(() => ({
    positionForRow: (row) => originalsByKey.get(row.key)?.position ?? 0,
    onMove: async (row, position) => {
      const original = originalFor(row);
      setNotice(null);
      setActionError(null);
      try {
        await moveInstanceContent(instanceId, original.itemId, position);
      } catch (error) {
        await refresh();
        throw new Error(`Could not move ${original.name}. ${errorMessage(error)}`);
      }
      await refresh();
    },
  }), [instanceId, originalsByKey, refresh]);

  const toggleRow = async (row: InstalledContentRow) => {
    const original = originalFor(row);
    setNotice(null);
    setActionError(null);
    try {
      await setInstanceContentEnabled(instanceId, original.itemId, !original.enabled);
      await refresh();
      return true;
    } catch (error) {
      await refresh();
      throw new Error(`Could not turn ${original.name} ${original.enabled ? 'off' : 'on'}. ${errorMessage(error)}`);
    }
  };

  const bulkToggle = async (rows: InstalledContentRow[], enabled: boolean) => {
    setNotice(null);
    setActionError(null);
    try {
      for (const row of originalsFor(rows)) {
        await setInstanceContentEnabled(instanceId, row.itemId, enabled);
      }
      await refresh();
      return true;
    } catch (error) {
      await refresh();
      throw new Error(`Could not change all the selected items. ${errorMessage(error)}`);
    }
  };

  const bulkRemove = async (rows: InstalledContentRow[]) => {
    const targets = originalsFor(rows);
    const confirmed = await confirm({
      title: `Remove ${targets.length} item${targets.length === 1 ? '' : 's'} from this instance?`,
      body: 'They stay in your library, so you can add them again later.',
      confirmLabel: 'Remove',
      tone: 'danger',
    });
    if (!confirmed) return false;
    setNotice(null);
    setActionError(null);
    try {
      for (const row of targets) {
        await removeInstanceContent(instanceId, row.itemId);
      }
      await refresh();
      return true;
    } catch (error) {
      await refresh();
      setActionError(`Could not remove all the selected items. ${errorMessage(error)}`);
      return false;
    }
  };

  const removeRow = async (row: InstalledContentRow) => {
    const original = originalFor(row);
    const confirmed = await confirm({
      title: `Remove ${original.name} from this instance?`,
      body: 'It stays in your library, so you can add it again later.',
      confirmLabel: 'Remove',
      tone: 'danger',
    });
    if (!confirmed) return;
    setNotice(null);
    setActionError(null);
    try {
      await removeInstanceContent(instanceId, original.itemId);
      await refresh();
    } catch (error) {
      await refresh();
      setActionError(`Could not remove ${original.name}. ${errorMessage(error)}`);
    }
  };

  const revealRow = async () => {
    setActionError(null);
    try {
      await openGameInstanceFolder(instanceId);
    } catch (error) {
      setActionError(`Could not open the instance folder. ${errorMessage(error)}`);
    }
  };

  const addSelected = async () => {
    const item = store?.find((candidate) => candidate.itemId === pickedId);
    if (!item) return;
    setNotice(null);
    setActionError(null);
    setBusy('add');
    try {
      const outcome = await addInstanceContent(instanceId, item.itemId);
      if (outcome.status === 'needs_installer') {
        setNotice(`${item.name} has an installer. Answer its questions to choose what gets added.`);
        setWizardFor(outcome.itemId);
      } else {
        setNotice(addedNotice(outcome, item.name));
        await refresh();
      }
      setPickedId('');
    } catch (error) {
      if (isGamesError(error) && error.code === 'placement_unknown') {
        const details = error.details as { topLevel?: unknown; note?: unknown } | undefined;
        if (typeof details?.note === 'string' && details.note !== '') {
          setPlacementNote({
            topLevel: Array.isArray(details.topLevel) ? details.topLevel.filter((entry): entry is string => typeof entry === 'string') : [],
            note: details.note,
          });
        } else {
          setActionError(`Could not add ${item.name}: ${errorMessage(error)}`);
        }
      } else {
        setActionError(`Could not add ${item.name}: ${errorMessage(error)}`);
      }
    } finally {
      setBusy(null);
    }
  };

  const runItemTool = async (tool: ToolRow, item: InstanceContentRow, values: Record<string, string>) => {
    setToolBusy(true);
    setToolError(null);
    try {
      if (tool.inputs.length > 0) await saveGameToolInputs(instanceId, tool.toolId, values);
      const result = await runGameTool(instanceId, tool.toolId, 'auto', { itemId: item.itemId });
      if (result.status === 'finished') {
        setNotice(`Ran ${tool.name} on ${item.name}.`);
        setToolTarget(null);
      } else if (result.status === 'vfsRefused') {
        setToolError(`${tool.name} could not run its usual way. ${result.reason}`);
      } else if (result.status === 'needsSwap') {
        setToolError(`${tool.name} works on the game's real install. Run it from the Tools tab.`);
      } else {
        setToolError(`${tool.name} could not run: its output folder already has files.`);
      }
    } catch (error) {
      setToolError(errorMessage(error));
    } finally {
      setToolBusy(false);
    }
  };

  const inInstance = new Set(items.map((row) => row.itemId));
  const forThisGame = itemsForInstanceOf(store ?? [], gameId);
  const addable = forThisGame.filter((item) => !inInstance.has(item.itemId));

  return (
    <section className="space-y-4" data-testid="instance-content">
      {notice && <p role="status" className="text-sm text-muted-foreground">{notice}</p>}
      {actionError && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          {actionError}
        </p>
      )}
      {itemToolsError && (
        <p role="status" className="text-xs text-muted-foreground">
          Tools that run on one item could not be read: {itemToolsError}
        </p>
      )}
      {toolError && (
        <p role="alert" className="rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          {toolError}
        </p>
      )}

      {contentError && (
        <div role="alert" className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-destructive/50 bg-destructive/10 p-3 text-sm">
          <span>This instance’s content could not be read: {contentError}</span>
          <Button variant="outline" size="sm" onClick={() => void refresh()}>Try again</Button>
        </div>
      )}

      {!contentError && content === null && <p className="text-sm text-muted-foreground">Reading the content…</p>}

      {content !== null && (
        <InstalledContentPanel
          contentType="mod"
          preferencesKey="agora.installed-content.v2.game"
          title="Content"
          contentLabel="content"
          description="What this instance uses, in load order. Items load from the top down, so when two change the same file, the lower one wins."
          rows={panelRows}
          locked={false}
          availableColumns={GAME_CONTENT_COLUMNS}
          columnLabels={GAME_CONTENT_COLUMN_LABELS}
          defaultColumns={GAME_CONTENT_COLUMNS}
          availableFilters={GAME_CONTENT_FILTERS}
          rowTestId="instance-content-row"
          rowSubtitle={(row) => {
            const original = originalsByKey.get(row.key);
            return original ? whereItGoes(original) : null;
          }}
          emptyState={(
            <p className="mt-4 rounded-xl border border-dashed border-border p-4 text-sm text-muted-foreground">
              This instance has no content yet. Add something from your library below. If your library is empty, add an
              archive or folder on the Library tab first.
            </p>
          )}
          onToggle={toggleRow}
          onBulkToggle={bulkToggle}
          onBulkRemove={bulkRemove}
          onRemove={(row) => void removeRow(row)}
          onRevealFile={() => void revealRow()}
          revealLabel="Open instance folder"
          canRevealFile={() => true}
          showCopyFilename={false}
          onError={setActionError}
          loadOrder={loadOrder}
          extraRowMenuItems={(row) => {
            const original = originalsByKey.get(row.key);
            if (!original || itemTools.length === 0) return null;
            return itemTools.map((tool) => (
              <DropdownMenuItem key={tool.toolId} onSelect={() => {
                setToolError(null);
                setToolTarget({ tool, item: original });
              }}>
                Run {tool.name} on this
              </DropdownMenuItem>
            ));
          }}
        />
      )}

      <div className="space-y-2 rounded-xl border border-border p-4">
        <h3 className="text-sm font-medium">Add from your library</h3>
        <p className="text-xs text-muted-foreground">Items for this game, and items not tied to any game.</p>
        {storeError && (
          <p role="alert" className="text-sm">Your library could not be read, so nothing can be added: {storeError}</p>
        )}
        {!storeError && store !== null && addable.length === 0 && (
          <p className="text-sm text-muted-foreground">
            {forThisGame.length === 0
              ? 'Your library has nothing for this game yet. Add an archive or folder on the Library tab first.'
              : 'Everything in your library for this game is already in this instance.'}
          </p>
        )}
        {addable.length > 0 && (
          <div className="flex flex-wrap items-center gap-2">
            <label className="sr-only" htmlFor="instance-content-add">Choose an item to add</label>
            <select
              id="instance-content-add"
              className="h-10 min-w-0 flex-1 rounded-md border border-input bg-background px-3 text-sm"
              value={pickedId}
              onChange={(event) => setPickedId(event.target.value)}
            >
              <option value="">Choose an item…</option>
              {addable.map((item) => (
                <option key={item.itemId} value={item.itemId}>{item.name}</option>
              ))}
            </select>
            <Button onClick={() => void addSelected()} disabled={pickedId === '' || busy !== null}>
              Add to instance
            </Button>
          </div>
        )}
      </div>

      <Dialog open={placementNote !== null} onOpenChange={(open) => { if (!open) setPlacementNote(null); }}>
        <DialogContent data-testid="placement-unknown">
          <DialogHeader>
            <DialogTitle>Agora cannot tell where these files go</DialogTitle>
            <DialogDescription>
              {placementNote !== null && placementNote.topLevel.length > 0
                ? `Top-level entries: ${placementNote.topLevel.join(', ')}`
                : 'This game has no rule for these files.'}
            </DialogDescription>
          </DialogHeader>
          {placementNote !== null && (
            <p className="text-sm text-muted-foreground">{placementNote.note}</p>
          )}
          <DialogFooter>
            <Button type="button" onClick={() => setPlacementNote(null)}>Close</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={toolTarget !== null} onOpenChange={(open) => { if (!open && !toolBusy) setToolTarget(null); }}>
        <DialogContent data-testid="item-tool-dialog">
          <DialogHeader>
            <DialogTitle>{toolTarget === null ? 'Run a tool' : `Run ${toolTarget.tool.name} on ${toolTarget.item.name}`}</DialogTitle>
            <DialogDescription>
              {toolTarget?.tool.inputs.length
                ? 'Fill in what the tool needs, then run it on this item.'
                : 'This tool runs on this item only.'}
            </DialogDescription>
          </DialogHeader>
          {toolTarget !== null && toolTarget.tool.inputs.length > 0 ? (
            <ToolInputsForm
              inputs={toolTarget.tool.inputs}
              initialValues={toolTarget.tool.inputValues}
              busy={toolBusy}
              submitLabel={`Run ${toolTarget.tool.name}`}
              onSubmit={(values) => runItemTool(toolTarget.tool, toolTarget.item, values)}
              onCancel={() => setToolTarget(null)}
            />
          ) : (
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setToolTarget(null)} disabled={toolBusy}>Cancel</Button>
              <Button type="button" disabled={toolBusy} onClick={() => {
                if (toolTarget !== null) void runItemTool(toolTarget.tool, toolTarget.item, {});
              }}>
                Run {toolTarget?.tool.name}
              </Button>
            </DialogFooter>
          )}
        </DialogContent>
      </Dialog>

      {wizardFor && (
        <FomodWizard
          contentId={wizardFor}
          instanceId={instanceId}
          onClose={() => {
            setWizardFor(null);
            void refresh();
            void refreshLibrary();
          }}
        />
      )}
    </section>
  );
}
