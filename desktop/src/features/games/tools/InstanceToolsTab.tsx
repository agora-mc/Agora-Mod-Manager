/**
 * An instance's Tools page (MASTER_SPEC §26.9): the tools present in this instance, and nothing
 * else. A tool the game recognises (Nemesis, BodySlide…) is listed when a mod installs it, by the
 * core's detection; a Mod Organizer 2 import brings its executables list; and the player adds any
 * other program with "Add a tool…", or picks from what "Scan for programs" finds. Each tool's own actions live in ToolCard. The swap banner shows
 * when a swap was interrupted.
 */

import { useCallback, useEffect, useState } from 'react';
import { Plus, ScanSearch, Wrench } from 'lucide-react';
import { EmptyState, PageHeroAction } from '@/components/instance-kit';
import {
  gameToolSwapStatus,
  listGameTools,
  listUserTools,
  removeUserTool,
  restoreGameToolSwap,
  type SwapStatus,
  type ToolRow,
  type ToolsView,
  type UserToolRow,
} from './api';
import { AddToolPanel } from './AddToolPanel';
import { ToolCard } from './ToolCard';
import { ToolScanPanel } from './ToolScanPanel';
import { addedNote, conflictLine, emptyToolsBody, messageOf, originLine, toolsSummary } from './toolCopy';

export function InstanceToolsTab({ instanceId }: { gameId: string; instanceId: string }) {
  const [view, setView] = useState<ToolsView | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [swap, setSwap] = useState<SwapStatus | null>(null);
  const [swapError, setSwapError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [restoreMessage, setRestoreMessage] = useState<string | null>(null);

  const [userTools, setUserTools] = useState<UserToolRow[] | null>(null);
  const [userToolsError, setUserToolsError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [scanning, setScanning] = useState(false);

  const loadTools = useCallback(async (isCurrent: () => boolean) => {
    setLoadError(null);
    setUserToolsError(null);
    const [listed, users] = await Promise.allSettled([listGameTools(instanceId), listUserTools(instanceId)]);
    if (!isCurrent()) return;
    if (listed.status === 'fulfilled') setView(listed.value);
    else { setView(null); setLoadError(messageOf(listed.reason)); }
    if (users.status === 'fulfilled') setUserTools(users.value.tools);
    else { setUserTools(null); setUserToolsError(messageOf(users.reason)); }
  }, [instanceId]);

  const loadSwap = useCallback(async (isCurrent: () => boolean) => {
    setSwapError(null);
    try {
      const status = await gameToolSwapStatus(instanceId);
      if (isCurrent()) setSwap(status);
    } catch (error) {
      if (isCurrent()) { setSwap(null); setSwapError(messageOf(error)); }
    }
  }, [instanceId]);

  useEffect(() => {
    let current = true;
    void loadTools(() => current);
    void loadSwap(() => current);
    return () => { current = false; };
  }, [loadTools, loadSwap]);

  const reload = () => {
    void loadTools(() => true);
    void loadSwap(() => true);
  };

  const restore = async () => {
    setBusy(true);
    setRestoreMessage(null);
    try {
      const reply = await restoreGameToolSwap(instanceId);
      setRestoreMessage(reply.restoredTools.length > 0
        ? `The game’s Data folder is back where it belongs${reply.restoredTools.length === 1 ? ` (the swap for ${reply.restoredTools[0]})` : ''}.`
        : 'Nothing needed putting back.');
      reload();
    } catch (error) {
      setRestoreMessage(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const removeUser = async (row: UserToolRow) => {
    setBusy(true);
    setUserToolsError(null);
    setNote(null);
    try {
      const reply = await removeUserTool(instanceId, row.id);
      setUserTools(reply.tools);
      setNote(`Removed ${row.name} from the list. No files were deleted.`);
    } catch (error) {
      setUserToolsError(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const openAdd = () => {
    setNote(null);
    setScanning(false);
    setAdding(true);
  };

  const openScan = () => {
    setNote(null);
    setAdding(false);
    setScanning(true);
  };

  const interrupted = swap?.interrupted ?? null;
  const gameTools = view?.tools ?? [];
  const listedToolIds = gameTools.map((row) => row.toolId);
  // A tool the player added that the game recognises runs as that tool: when the list already shows
  // it, it is one row, not two.
  const ownTools = (userTools ?? []).filter((row) => !(row.knownToolId && listedToolIds.includes(row.knownToolId)));
  const loaded = view !== null && userTools !== null;
  const count = gameTools.length + ownTools.length;
  const empty = loaded && count === 0;

  return (
    <section className="space-y-4 rounded-xl border border-border bg-card p-4" aria-labelledby="tools-heading" data-testid="instance-tools">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div className="min-w-0">
          <h2 id="tools-heading" className="text-base font-semibold">
            Tools{loaded ? ` (${count})` : ''}
          </h2>
          <p className="mt-1 max-w-2xl text-xs text-muted-foreground">{toolsSummary(view?.known ?? [])}</p>
        </div>
        {!empty && !adding && !scanning && (
          <div className="flex shrink-0 flex-wrap gap-2 self-start sm:self-auto">
            <button
              type="button"
              onClick={openScan}
              className="inline-flex items-center gap-1.5 rounded-lg border border-input bg-background px-3 py-1.5 text-sm font-medium hover:bg-accent"
            >
              <ScanSearch className="h-4 w-4" aria-hidden="true" />
              Scan for programs
            </button>
            <button
              type="button"
              onClick={openAdd}
              className="inline-flex items-center gap-1.5 rounded-lg border border-input bg-background px-3 py-1.5 text-sm font-medium hover:bg-accent"
            >
              <Plus className="h-4 w-4" aria-hidden="true" />
              Add a tool…
            </button>
          </div>
        )}
      </div>

      {swapError && (
        <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">
          Agora could not check for an interrupted swap, so it cannot say whether the game’s Data folder is in
          place: {swapError}
        </p>
      )}

      {interrupted && (
        <section role="alert" aria-label="Interrupted tool swap" className="space-y-2 rounded-xl border border-destructive bg-destructive/10 p-4 text-sm" data-testid="tool-swap-recovery">
          <h3 className="font-semibold">A tool swap was interrupted</h3>
          <p>
            The game’s real Data folder may be renamed aside while a tool runs on a copy. Put it back before you launch
            the game or run another tool.
          </p>
          <p>
            {interrupted.tool
              ? `The swap was for ${interrupted.tool}.`
              : 'Agora cannot read which tool the swap was for.'}
            {interrupted.unreadable ? ` The saved record cannot be read: ${interrupted.unreadable}` : ''}
          </p>
          {interrupted.running ? (
            <p>A tool is still running from the game folder. Let it finish or close it, then try again.</p>
          ) : (
            <button
              type="button"
              disabled={busy}
              onClick={() => void restore()}
              className="rounded-lg bg-primary px-3 py-1.5 font-medium text-primary-foreground shadow-sm disabled:opacity-50"
            >
              Put the Data folder back
            </button>
          )}
        </section>
      )}
      {restoreMessage && <p role="status" className="text-sm text-muted-foreground">{restoreMessage}</p>}

      {adding && (
        <AddToolPanel
          instanceId={instanceId}
          listedToolIds={listedToolIds}
          onAdded={(name) => {
            setAdding(false);
            setNote(`Added ${name}.`);
            void loadTools(() => true);
          }}
          onClose={() => setAdding(false)}
        />
      )}
      {scanning && (
        <ToolScanPanel
          instanceId={instanceId}
          listedToolIds={listedToolIds}
          onAdded={(names, keepOpen) => {
            if (!keepOpen) setScanning(false);
            setNote(addedNote(names));
            void loadTools(() => true);
          }}
          onClose={() => setScanning(false)}
        />
      )}
      {note && <p role="status" className="text-sm text-muted-foreground">{note}</p>}

      {!loaded && !loadError && !userToolsError && (
        <p className="text-sm text-muted-foreground">Looking at this instance’s tools…</p>
      )}

      {loadError && (
        <div role="alert" className="space-y-2 rounded-xl border border-destructive bg-destructive/10 p-4 text-sm">
          <p>The tools could not be listed: {loadError}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent"
          >
            Try again
          </button>
        </div>
      )}
      {userToolsError && (
        <div role="alert" className="space-y-2 rounded-xl border border-destructive bg-destructive/10 p-4 text-sm" data-testid="user-tools-error">
          <p>The tools you added could not be listed: {userToolsError}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent"
          >
            Try again
          </button>
        </div>
      )}

      {empty && !adding && !scanning && (
        <EmptyState
          testId="tools-empty"
          icon={Wrench}
          title="No tools yet"
          body={emptyToolsBody(view?.known ?? [])}
          actions={(
            <>
              <PageHeroAction primary icon={ScanSearch} onClick={openScan}>Scan for programs</PageHeroAction>
              <PageHeroAction icon={Plus} onClick={openAdd}>Add a tool…</PageHeroAction>
            </>
          )}
        />
      )}

      {gameTools.map((row) => (
        <div key={row.toolId} className="space-y-2">
          {row.conflicts && (
            <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm" data-testid="tool-conflict">
              {conflictLine(row, gameTools)}
            </p>
          )}
          <ToolCard
            instanceId={instanceId}
            row={row}
            busy={busy}
            setBusy={setBusy}
            onView={setView}
            onChanged={reload}
            origin={row.installed ? originLine(row) : null}
            exe={row.exe}
            unavailable={row.installed
              ? null
              : `${row.name} is no longer installed here: the mod that had it is off or was removed. Its output stays until you remove it.`}
          />
        </div>
      ))}

      {ownTools.map((row) => {
        const toolRow: ToolRow = {
          toolId: row.toolId,
          name: row.name,
          declared: true,
          current: null,
          previous: null,
          status: null,
          conflicts: false,
          outputKind: row.outputIsMod ? 'mod' : 'generated_layer',
          outputFolder: row.outputFolder,
          installed: !row.missing,
          exe: row.exe,
          fromMod: null,
          runOnly: !row.keepsOutput && !row.outputIsMod,
        };
        const details = [
          row.args.length > 0 ? `starts with ${row.args.join(' ')}` : null,
          row.outputIsMod && row.outputFolder ? `its output is a mod in ${row.outputFolder}` : null,
        ].filter(Boolean).join(' · ');
        return (
          <div key={row.id} data-testid="user-tool-card">
            <ToolCard
              instanceId={instanceId}
              row={toolRow}
              busy={busy}
              setBusy={setBusy}
              onView={setView}
              onChanged={reload}
              userToolId={row.id}
              origin={details ? `Added by you · ${details}` : 'Added by you'}
              exe={row.exe}
              unavailable={row.missing
                ? `${row.name}’s program is not there now: the mod that has it is off or was removed, or the file was moved. Turn the mod back on, or remove the tool from the list.`
                : null}
              headerActions={(
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void removeUser(row)}
                  className="rounded-lg border border-border bg-card px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:opacity-50"
                >
                  Remove from list
                </button>
              )}
            />
          </div>
        );
      })}
    </section>
  );
}
