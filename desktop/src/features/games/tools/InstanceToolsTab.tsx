/**
 * An instance's Tools page (MASTER_SPEC §26.9): the tools the game declares, the output each has
 * here, the tools the person added from their mods, and the swap banner when a swap was interrupted.
 * Each declared tool's own actions live in ToolCard.
 */

import { useCallback, useEffect, useState } from 'react';
import {
  addUserTool,
  findUserTools,
  gameToolSwapStatus,
  listGameTools,
  listUserTools,
  removeUserTool,
  restoreGameToolSwap,
  type SwapStatus,
  type ToolCandidate,
  type ToolRow,
  type ToolsView,
  type UserToolRow,
} from './api';
import { ToolCard } from './ToolCard';
import { conflictLine, messageOf } from './toolCopy';

export function InstanceToolsTab({ instanceId }: { gameId: string; instanceId: string }) {
  const [view, setView] = useState<ToolsView | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [swap, setSwap] = useState<SwapStatus | null>(null);
  const [swapError, setSwapError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [restoreMessage, setRestoreMessage] = useState<string | null>(null);

  const [userTools, setUserTools] = useState<UserToolRow[] | null>(null);
  const [userToolsError, setUserToolsError] = useState<string | null>(null);
  const [userNote, setUserNote] = useState<string | null>(null);
  const [candidates, setCandidates] = useState<ToolCandidate[] | null>(null);
  const [finding, setFinding] = useState(false);
  const [findError, setFindError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Record<string, boolean>>({});
  const [names, setNames] = useState<Record<string, string>>({});
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);
  const [newName, setNewName] = useState('');
  const [newExe, setNewExe] = useState('');
  const [newArgs, setNewArgs] = useState('');
  const [newIsMod, setNewIsMod] = useState(false);
  const [newFolder, setNewFolder] = useState('');
  const [addingManual, setAddingManual] = useState(false);

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

  const find = async () => {
    setFinding(true);
    setFindError(null);
    setAddError(null);
    setUserNote(null);
    try {
      const found = await findUserTools(instanceId);
      setCandidates(found);
      const nextSelected: Record<string, boolean> = {};
      const nextNames: Record<string, string> = {};
      for (const candidate of found) {
        nextSelected[candidate.exe] = !candidate.alreadyAdded;
        nextNames[candidate.exe] = candidate.suggestedName;
      }
      setSelected(nextSelected);
      setNames(nextNames);
    } catch (error) {
      setCandidates(null);
      setFindError(messageOf(error));
    } finally {
      setFinding(false);
    }
  };

  const addSelected = async () => {
    const checked = (candidates ?? []).filter((candidate) => selected[candidate.exe]);
    if (checked.length === 0) {
      setAddError('Choose at least one tool to add.');
      return;
    }
    setAdding(true);
    setAddError(null);
    const failures: string[] = [];
    let added = 0;
    for (const candidate of checked) {
      try {
        await addUserTool(instanceId, {
          name: (names[candidate.exe] ?? candidate.suggestedName).trim(),
          exe: candidate.exe,
          args: [],
        });
        added += 1;
      } catch (error) {
        failures.push(`${candidate.exe}: ${messageOf(error)}`);
      }
    }
    setAdding(false);
    if (added > 0) setUserNote(`Added ${added} ${added === 1 ? 'tool' : 'tools'}.`);
    if (failures.length > 0) setAddError(failures.join(' '));
    else setCandidates(null);
    void loadTools(() => true);
  };

  const addManual = async () => {
    setAddError(null);
    setUserNote(null);
    if (!newName.trim() || !newExe.trim()) {
      setAddError('A tool needs a name and an executable.');
      return;
    }
    if (newIsMod && !newFolder.trim()) {
      setAddError('A tool whose output is a mod needs an output folder.');
      return;
    }
    setAddingManual(true);
    try {
      await addUserTool(instanceId, {
        name: newName.trim(),
        exe: newExe.trim(),
        args: newArgs.trim() === '' ? [] : newArgs.trim().split(/\s+/),
        outputIsMod: newIsMod,
        outputFolder: newIsMod ? newFolder.trim() : null,
      });
      setUserNote(`Added ${newName.trim()}.`);
      setNewName('');
      setNewExe('');
      setNewArgs('');
      setNewIsMod(false);
      setNewFolder('');
      void loadTools(() => true);
    } catch (error) {
      setAddError(messageOf(error));
    } finally {
      setAddingManual(false);
    }
  };

  const removeUser = async (row: UserToolRow) => {
    setBusy(true);
    setUserToolsError(null);
    setUserNote(null);
    try {
      const reply = await removeUserTool(instanceId, row.id);
      setUserTools(reply.tools);
      setUserNote(`Removed ${row.name}.`);
    } catch (error) {
      setUserToolsError(messageOf(error));
    } finally {
      setBusy(false);
    }
  };

  const interrupted = swap?.interrupted ?? null;

  return (
    <section className="space-y-4" aria-labelledby="tools-heading" data-testid="instance-tools">
      <header className="space-y-1">
        <h2 id="tools-heading" className="text-lg font-semibold">Tools</h2>
        <p className="text-sm text-muted-foreground">
          Tools write files the game reads, such as animation or body files. Each run’s output is kept as a
          generation, so you can roll back, compare or remove it without touching your mods.
        </p>
      </header>

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

      {!view && !loadError && <p className="text-sm text-muted-foreground">Looking at this instance’s tools…</p>}

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

      {view && view.tools.length === 0 && (
        <p className="rounded-xl border border-dashed border-border p-6 text-sm text-muted-foreground" data-testid="tools-empty">
          This game declares no tools, so there is nothing here to build. Tools appear when a game package provides them.
        </p>
      )}

      {view && view.tools.map((row) => (
        <div key={row.toolId} className="space-y-2">
          {row.conflicts && (
            <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm" data-testid="tool-conflict">
              {conflictLine(row, view.tools)}
            </p>
          )}
          <ToolCard
            instanceId={instanceId}
            row={row}
            busy={busy}
            setBusy={setBusy}
            onView={setView}
            onChanged={reload}
          />
        </div>
      ))}

      <section aria-labelledby="user-tools-heading" className="space-y-3" data-testid="user-tools">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h3 id="user-tools-heading" className="text-base font-semibold">Your tools</h3>
          <button
            type="button"
            disabled={finding}
            onClick={() => void find()}
            className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:opacity-50"
          >
            {finding ? 'Looking through your mods…' : 'Find tools'}
          </button>
        </div>
        <p className="text-sm text-muted-foreground">
          Tools you add from your mods, with the arguments you want them to start with.
        </p>

        {userToolsError && (
          <div role="alert" className="space-y-2 rounded-xl border border-destructive bg-destructive/10 p-4 text-sm" data-testid="user-tools-error">
            <p>Your own tools could not be listed: {userToolsError}</p>
            <button
              type="button"
              onClick={reload}
              className="rounded-lg border border-border px-3 py-1.5 font-medium hover:bg-accent"
            >
              Try again
            </button>
          </div>
        )}

        {userTools && userTools.length === 0 && !userToolsError && (
          <p className="rounded-xl border border-dashed border-border p-6 text-sm text-muted-foreground" data-testid="user-tools-empty">
            No tools of your own yet. Use Find tools to look through this instance’s mods.
          </p>
        )}

        {userTools && userTools.map((row) => {
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
          };
          return (
            <div key={row.id} className="space-y-2" data-testid="user-tool-card">
              <ToolCard
                instanceId={instanceId}
                row={toolRow}
                busy={busy}
                setBusy={setBusy}
                onView={setView}
                onChanged={reload}
                userToolId={row.id}
              />
              <div className="flex flex-wrap items-center gap-3 px-1 text-sm">
                <p className="break-all text-muted-foreground">{row.exe}</p>
                {row.args.length > 0 && (
                  <p className="break-all text-muted-foreground">Starts with: {row.args.join(' ')}</p>
                )}
                {row.outputIsMod && row.outputFolder && (
                  <p className="break-all text-muted-foreground">Its output is a mod in {row.outputFolder}</p>
                )}
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void removeUser(row)}
                  className="rounded-lg border border-destructive px-3 py-1.5 font-medium text-destructive hover:bg-destructive/10 disabled:opacity-50"
                >
                  Remove
                </button>
              </div>
            </div>
          );
        })}

        <section aria-label="Add a tool of your own" className="space-y-3 rounded-xl border border-border p-4" data-testid="user-tool-add">
          <p className="text-sm font-medium">Add a tool of your own</p>
          <p className="text-xs text-muted-foreground">
            The executable can be a file this instance deploys, or an absolute path to a .exe outside the game.
          </p>
          <div className="grid gap-2 sm:grid-cols-2">
            <label className="flex flex-col gap-1 text-sm">
              <span className="text-muted-foreground">Name</span>
              <input
                type="text"
                value={newName}
                onChange={(event) => setNewName(event.target.value)}
                className="rounded-lg border border-border bg-background px-2 py-1"
              />
            </label>
            <label className="flex flex-col gap-1 text-sm">
              <span className="text-muted-foreground">Executable</span>
              <input
                type="text"
                value={newExe}
                placeholder="Data/tools/My.exe or C:/Modding/Tool/My.exe"
                onChange={(event) => setNewExe(event.target.value)}
                className="rounded-lg border border-border bg-background px-2 py-1"
              />
            </label>
            <label className="flex flex-col gap-1 text-sm">
              <span className="text-muted-foreground">Arguments (optional)</span>
              <input
                type="text"
                value={newArgs}
                onChange={(event) => setNewArgs(event.target.value)}
                className="rounded-lg border border-border bg-background px-2 py-1"
              />
            </label>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={newIsMod}
                onChange={(event) => setNewIsMod(event.target.checked)}
              />
              Its output is a mod
            </label>
            {newIsMod && (
              <label className="flex flex-col gap-1 text-sm sm:col-span-2">
                <span className="text-muted-foreground">Output folder (relative to the executable, or absolute)</span>
                <input
                  type="text"
                  value={newFolder}
                  placeholder="MyTool_Output"
                  onChange={(event) => setNewFolder(event.target.value)}
                  className="rounded-lg border border-border bg-background px-2 py-1"
                />
                <span className="text-xs text-muted-foreground">
                  Use the same output folder you set in PGPatcher’s settings. It must be outside the game folder and this instance’s deployment.
                </span>
              </label>
            )}
          </div>
          <button
            type="button"
            disabled={addingManual}
            onClick={() => void addManual()}
            className="rounded-lg bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground shadow-sm disabled:opacity-50"
          >
            Add this tool
          </button>
        </section>

        {userNote && <p role="status" className="text-sm text-muted-foreground">{userNote}</p>}
        {findError && (
          <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">
            Your mods could not be searched: {findError}
          </p>
        )}

        {candidates && (
          <section aria-label="Tools found in your mods" className="space-y-3 rounded-xl border border-border p-4" data-testid="tool-candidates">
            {candidates.length === 0 ? (
              <p className="text-sm text-muted-foreground">No tools found in this instance’s mods</p>
            ) : (
              <>
                <p className="text-sm font-medium">Choose the tools to add. Nothing is added until you say so.</p>
                {candidates.map((candidate) => (
                  <div key={candidate.exe} className="space-y-2 rounded-lg border border-border p-3 text-sm" data-testid="tool-candidate">
                    <div className="flex flex-wrap items-center gap-2">
                      <input
                        type="checkbox"
                        aria-label={`Add ${candidate.exe}`}
                        checked={selected[candidate.exe] ?? false}
                        disabled={candidate.alreadyAdded}
                        onChange={(event) => setSelected((current) => ({ ...current, [candidate.exe]: event.target.checked }))}
                      />
                      <span className="break-all font-medium">{candidate.exe}</span>
                      {candidate.knownToolId && (
                        <span className="rounded-full border border-border px-2.5 py-0.5 text-xs text-muted-foreground">
                          Known tool
                        </span>
                      )}
                      {candidate.alreadyAdded && (
                        <span className="rounded-full border border-border px-2.5 py-0.5 text-xs text-muted-foreground">
                          Already added
                        </span>
                      )}
                    </div>
                    {candidate.modName && <p className="text-xs text-muted-foreground">From {candidate.modName}</p>}
                    <label className="flex flex-wrap items-center gap-2">
                      <span className="text-muted-foreground">Name</span>
                      <input
                        type="text"
                        value={names[candidate.exe] ?? candidate.suggestedName}
                        disabled={candidate.alreadyAdded}
                        onChange={(event) => setNames((current) => ({ ...current, [candidate.exe]: event.target.value }))}
                        className="rounded-lg border border-border bg-background px-2 py-1"
                      />
                    </label>
                  </div>
                ))}
                <div className="flex flex-wrap gap-2">
                  <button
                    type="button"
                    disabled={adding || !candidates.some((candidate) => selected[candidate.exe])}
                    onClick={() => void addSelected()}
                    className="rounded-lg bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground shadow-sm disabled:opacity-50"
                  >
                    Add selected
                  </button>
                  <button
                    type="button"
                    onClick={() => setCandidates(null)}
                    className="rounded-lg border border-border px-3 py-1.5 text-sm font-medium hover:bg-accent"
                  >
                    Close
                  </button>
                </div>
              </>
            )}
            {addError && (
              <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">{addError}</p>
            )}
          </section>
        )}
      </section>
    </section>
  );
}
