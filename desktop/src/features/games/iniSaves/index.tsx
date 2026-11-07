/**
 * The "INI & saves" tab of a non-Minecraft game's instance (MASTER_SPEC §26.7): the game's own
 * configuration files, edited per instance, and the folder the game keeps its saves in.
 *
 * Each section loads and fails on its own, so a problem with one never hides the other.
 */

import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useConfirm } from '@/components/ui/confirm';
import { useOptionalInstanceSession } from '../instances';
import {
  describeFailure,
  getIni,
  getModIni,
  getSaves,
  keepModIniGameVersion,
  listModInis,
  resetModIni,
  restoreModIniMine,
  setModIniValue,
  setIniValue,
  setSaves,
  type IniChange,
  type IniContents,
  type IniEntryRow,
  type IniFileRow,
  type IniSourceName,
  type ModIniRow,
  type SaveChoice,
  type SaveFolderRow,
  type SavesChange,
} from './api';

interface InstanceIniSavesTabProps {
  gameId: string;
  instanceId: string;
}

export function InstanceIniSavesTab({ instanceId }: InstanceIniSavesTabProps) {
  return (
    <section className="space-y-8" aria-label="INI and saves" data-testid="ini-saves-tab">
      <SavesSection instanceId={instanceId} />
      <IniSection instanceId={instanceId} />
      <ModIniSection instanceId={instanceId} />
    </section>
  );
}

type ReadState<T> = { status: 'loading' } | { status: 'failed'; message: string } | { status: 'loaded'; value: T };

/**
 * Runs `read` for the instance on mount and again when the instance changes. A reply for an older
 * request is dropped, so a slow answer for the last instance never replaces the current one.
 */
function useReadOnMount<T>(read: (instanceId: string) => Promise<T>, instanceId: string) {
  const [state, setState] = useState<ReadState<T>>({ status: 'loading' });
  const requestId = useRef(0);

  const refetch = useCallback(() => {
    const id = ++requestId.current;
    setState({ status: 'loading' });
    read(instanceId)
      .then((value) => {
        if (id === requestId.current) setState({ status: 'loaded', value });
      })
      .catch((failure: unknown) => {
        if (id === requestId.current) setState({ status: 'failed', message: describeFailure(failure) });
      });
  }, [read, instanceId]);

  useEffect(() => {
    refetch();
    return () => {
      requestId.current++;
    };
  }, [refetch]);

  return { state, refetch, setState };
}

function SectionFrame({ title, id, children }: { title: string; id: string; children: ReactNode }) {
  return (
    <section aria-labelledby={id} className="space-y-4">
      <h2 id={id} className="text-lg font-semibold">
        {title}
      </h2>
      {children}
    </section>
  );
}

function FailureNote({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="space-y-3">
      <p role="alert" className="rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
        {message}
      </p>
      <Button variant="outline" size="sm" onClick={onRetry}>
        Try again
      </Button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Save folder
// ---------------------------------------------------------------------------

/** The shared folder is named after the game when the page knows it: "Skyrim's usual save folder". */
function saveChoicePlain(choice: SaveChoice, gameName: string | null): string {
  if (choice === 'shared' && gameName !== null) {
    return `${gameName}'s usual save folder, which every instance that uses shared saves reads.`;
  }
  return SAVE_CHOICE_WORDS[choice].plain;
}

const SAVE_CHOICE_WORDS: Record<SaveChoice, { label: string; plain: string }> = {
  shared: {
    label: 'Shared saves',
    plain: "The game's usual save folder, which every instance that uses shared saves reads.",
  },
  own: {
    label: 'Own saves',
    plain: "A save folder of this instance's own, so its saves stay apart from the shared ones.",
  },
};

/** A save time as people read it; anything that does not parse is shown as it came. */
export function readableWhen(value: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(parsed);
}

export function folderLine(folder: SaveFolderRow): string {
  if (!folder.exists) return 'Not made yet. The game creates it.';
  if (folder.saves === 0) return 'No saves yet.';
  const count = `${folder.saves} ${folder.saves === 1 ? 'save' : 'saves'}`;
  return folder.newest ? `${count}, newest ${readableWhen(folder.newest)}` : count;
}

function FolderPath({ folder }: { folder: SaveFolderRow }) {
  return (
    <div className="space-y-1">
      <p className="break-all rounded-md border border-border bg-muted/60 px-2.5 py-1.5 font-mono text-xs" title={folder.path}>
        {folder.path}
      </p>
      <p className="text-xs text-muted-foreground">{folderLine(folder)}</p>
    </div>
  );
}

function settingSourceWords(source: IniSourceName): string {
  if (source === 'copy') return "the instance's copy";
  if (source === 'game_file') return "the game's file";
  return 'no file yet';
}

/** What a change of the save choice did to the game's setting, in plain words. */
function settingNotice(change: SavesChange): string {
  const { action, value } = change.setting;
  switch (action) {
    case 'set':
      return `The game's save setting now names ${value ?? 'that folder'}.`;
    case 'unchanged':
      return "The game's save setting already named that folder.";
    case 'restored':
      return `The game's save setting is back to ${value ?? 'its old value'}, as it was before.`;
    case 'removed':
      return "The game's save setting was removed; it was not set before.";
    case 'left_alone':
      return `The game's save setting no longer holds the value Agora set (it holds ${value ?? 'nothing'}), so it was left as it is. Check it by hand.`;
    case 'nothing_recorded':
      return "Nothing was recorded for this instance, so the game's save setting was left as it is.";
  }
}

function SavesSection({ instanceId }: { instanceId: string }) {
  const { state, refetch, setState } = useReadOnMount(getSaves, instanceId);
  const session = useOptionalInstanceSession();
  const gameName = session?.page.status === 'loaded' ? session.page.detail.gameName : null;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const choose = async (choice: SaveChoice) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const change = await setSaves(instanceId, choice);
      setState({ status: 'loaded', value: change.saves });
      const moved = change.previous === change.saves.choice
        ? `This instance already keeps ${SAVE_CHOICE_WORDS[choice].label.toLowerCase()}.`
        : `This instance now keeps ${SAVE_CHOICE_WORDS[choice].label.toLowerCase()}.`;
      setNotice(`${moved} ${settingNotice(change)} Your saves were not moved, copied or deleted.`);
    } catch (failure: unknown) {
      setError(describeFailure(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SectionFrame title="Save folder" id="ini-saves-folder-heading">
      {state.status === 'loading' && <p className="text-sm text-muted-foreground">Reading the save choice…</p>}
      {state.status === 'failed' && <FailureNote message={state.message} onRetry={refetch} />}
      {state.status === 'loaded' && (
        <div className="space-y-4" data-testid="saves-panel">
          <p className="max-w-2xl text-sm text-muted-foreground">
            The game keeps its saves in one of two folders. Switching changes which one this instance uses. It never
            moves a save: the saves in the other folder stay where they are.
          </p>
          <div className="grid gap-3 md:grid-cols-2">
            {(['shared', 'own'] as const).map((choice) => {
              const current = state.value.choice === choice;
              const folder = current ? state.value.inUse : state.value.other;
              return (
                <div
                  key={choice}
                  className={[
                    'flex flex-col gap-3 rounded-xl border p-4',
                    current ? 'border-primary bg-primary/5' : 'border-border bg-card',
                  ].join(' ')}
                  data-testid={`saves-choice-${choice}`}
                >
                  <div className="space-y-1">
                    <div className="flex items-center justify-between gap-2">
                      <h3 className="font-semibold">{SAVE_CHOICE_WORDS[choice].label}</h3>
                      {current && (
                        <span className="rounded-full bg-primary px-2 py-0.5 text-xs font-semibold text-primary-foreground">In use</span>
                      )}
                    </div>
                    <p className="text-sm text-muted-foreground">{saveChoicePlain(choice, gameName)}</p>
                  </div>
                  <FolderPath folder={folder} />
                  {!current && (
                    <Button className="self-start" variant="outline" size="sm" onClick={() => choose(choice)} disabled={busy}>
                      Use {SAVE_CHOICE_WORDS[choice].label.toLowerCase()}
                    </Button>
                  )}
                </div>
              );
            })}
          </div>
          <p className="text-xs text-muted-foreground" data-testid="saves-setting">
            The game finds the folder through{' '}
            <code className="rounded bg-muted px-1 py-0.5 font-mono">{state.value.setting}</code> in {state.value.settingFile}:{' '}
            {state.value.settingValue === null ? (
              'not set'
            ) : (
              <code className="rounded bg-muted px-1 py-0.5 font-mono">{state.value.settingValue}</code>
            )}{' '}
            (from {settingSourceWords(state.value.settingSource)}).
          </p>
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
    </SectionFrame>
  );
}

// ---------------------------------------------------------------------------
// INI files
// ---------------------------------------------------------------------------

function fileStatus(file: IniFileRow): string {
  if (file.copyExists) return "The instance's copy";
  if (file.gameFileExists) return "The game's file";
  return 'Not there yet';
}

/** The sections of a file, in the order they first appear, with their keys. */
function groupBySection(entries: IniEntryRow[]): Array<{ section: string; entries: IniEntryRow[] }> {
  const groups: Array<{ section: string; entries: IniEntryRow[] }> = [];
  for (const entry of entries) {
    const group = groups.find((candidate) => candidate.section === entry.section);
    if (group) group.entries.push(entry);
    else groups.push({ section: entry.section, entries: [entry] });
  }
  return groups;
}

function IniSection({ instanceId }: { instanceId: string }) {
  const { confirm } = useConfirm();
  const { state, refetch, setState } = useReadOnMount(getIni, instanceId);
  const [selected, setSelected] = useState<string | null>(null);
  const [contents, setContents] = useState<IniContents | null>(null);
  const [fileState, setFileState] = useState<'idle' | 'loading' | 'failed'>('idle');
  const [fileError, setFileError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // Pick the first file once the list arrives, and keep a file the user picked while it still exists.
  const files = state.status === 'loaded' ? state.value.files : [];
  const activeFile = selected !== null && files.some((file) => file.instancePath === selected)
    ? selected
    : files[0]?.instancePath ?? null;

  useEffect(() => {
    if (activeFile === null) {
      setContents(null);
      return;
    }
    let cancelled = false;
    setFileState('loading');
    setFileError(null);
    getIni(instanceId, activeFile)
      .then((view) => {
        if (cancelled) return;
        setContents(view.contents);
        setFileState('idle');
      })
      .catch((failure: unknown) => {
        if (cancelled) return;
        setFileError(describeFailure(failure));
        setFileState('failed');
      });
    return () => {
      cancelled = true;
    };
  }, [instanceId, activeFile]);

  /** Applies the reply to a key change: the file list and the file shown both come from it. */
  const applyChange = (change: IniChange) => {
    setState({ status: 'loaded', value: change.view });
    if (change.view.contents) setContents(change.view.contents);
  };

  const saveKey = async (file: string, section: string, key: string, value: string | null) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const change = await setIniValue(instanceId, file, section, key, value);
      applyChange(change);
      if (value === null) {
        setNotice(change.changed ? `Removed ${key} from [${section}].` : `${key} was not set in [${section}]; nothing changed.`);
      } else {
        setNotice(change.changed ? `Saved ${key} in [${section}].` : `${key} in [${section}] already has that value.`);
      }
      return true;
    } catch (failure: unknown) {
      setError(describeFailure(failure));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const removeKey = async (file: string, entry: IniEntryRow) => {
    const confirmed = await confirm({
      title: `Remove ${entry.key} from [${entry.section}]?`,
      body: `Agora takes this key out of ${file} for this instance only. The game then uses its own default for it.`,
      confirmLabel: 'Remove key',
      tone: 'danger',
    });
    if (!confirmed) return;
    await saveKey(file, entry.section, entry.key, null);
  };

  return (
    <SectionFrame title="Game settings (INI files)" id="ini-saves-ini-heading">
      {state.status === 'loading' && <p className="text-sm text-muted-foreground">Reading the INI files…</p>}
      {state.status === 'failed' && <FailureNote message={state.message} onRetry={refetch} />}
      {state.status === 'loaded' && files.length === 0 && (
        <p className="rounded-xl border border-dashed border-border bg-card/60 p-4 text-sm text-muted-foreground">
          This game keeps no per-user INI files for this instance's store, so there is nothing to edit here.
        </p>
      )}
      {state.status === 'loaded' && files.length > 0 && (
        <div className="space-y-4" data-testid="ini-panel">
          <p className="max-w-2xl text-sm text-muted-foreground">
            These are the game's settings files. Changing a key here changes this instance only. The first change makes
            the instance's own copy; the game's file is not changed.
          </p>
          <div className="flex flex-wrap gap-2" role="group" aria-label="INI files">
            {files.map((file) => {
              const isActive = file.instancePath === activeFile;
              return (
                <button
                  key={file.instancePath}
                  type="button"
                  aria-pressed={isActive}
                  onClick={() => setSelected(file.instancePath)}
                  className={[
                    'rounded-lg px-3 py-1.5 text-left text-sm transition-colors',
                    isActive
                      ? 'bg-primary text-primary-foreground shadow-sm'
                      : 'border border-border text-muted-foreground hover:bg-accent hover:text-accent-foreground',
                  ].join(' ')}
                >
                  <span className="font-mono">{file.instancePath}</span>{' '}
                  <span className="text-xs opacity-80">({fileStatus(file)})</span>
                </button>
              );
            })}
          </div>

          {fileState === 'loading' && <p className="text-sm text-muted-foreground">Reading the file…</p>}
          {fileState === 'failed' && fileError && (
            <p role="alert" className="rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
              {fileError}
            </p>
          )}
          {fileState === 'idle' && contents && (
            <IniFileEditor
              contents={contents}
              busy={busy}
              onSaveKey={(section, key, value) =>
                saveKey(contents.instancePath, section, key, value)
              }
              onRemoveKey={(entry) => removeKey(contents.instancePath, entry)}
            />
          )}
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
    </SectionFrame>
  );
}

interface IniKeyRowProps {
  entry: IniEntryRow;
  disabled: boolean;
  onSave: (value: string) => Promise<boolean>;
  onRemove: () => void;
}

function IniKeyRow({ entry, disabled, onSave, onRemove }: IniKeyRowProps) {
  const [draft, setDraft] = useState(entry.value);

  // A change that came back from the backend replaces what the field showed.
  useEffect(() => {
    setDraft(entry.value);
  }, [entry.value]);

  return (
    <li className="flex flex-wrap items-center gap-2 px-3 py-2">
      <span className="min-w-40 font-mono text-sm">{entry.key}</span>
      <Input
        aria-label={`Value of ${entry.key} in [${entry.section}]`}
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
        className="h-9 min-w-48 flex-1 font-mono text-sm"
        disabled={disabled}
      />
      <Button size="sm" onClick={() => void onSave(draft)} disabled={disabled || draft === entry.value}>
        Save
      </Button>
      <Button size="sm" variant="outline" onClick={onRemove} disabled={disabled}>
        Remove
      </Button>
    </li>
  );
}

// ---------------------------------------------------------------------------
// The per-key editor, shared by the game's INI files and the mods' INI files
// ---------------------------------------------------------------------------

interface IniFileEditorProps {
  contents: IniContents;
  busy: boolean;
  readOnly?: boolean;
  note?: ReactNode;
  onSaveKey: (section: string, key: string, value: string | null) => Promise<boolean>;
  onRemoveKey: (entry: IniEntryRow) => void;
}

function IniFileEditor({ contents, busy, readOnly = false, note, onSaveKey, onRemoveKey }: IniFileEditorProps) {
  const [newSection, setNewSection] = useState('');
  const [newKey, setNewKey] = useState('');
  const [newValue, setNewValue] = useState('');

  const addKey = async () => {
    if (readOnly) return;
    const saved = await onSaveKey(newSection.trim(), newKey.trim(), newValue);
    if (saved) {
      setNewKey('');
      setNewValue('');
    }
  };

  return (
    <div className="space-y-3">
      {note}
      {contents.source === 'game_file' && (
        <p className="text-sm text-muted-foreground">
          Showing the game's own file, as the instance has no copy yet. Saving a key makes the instance's copy from
          it.
        </p>
      )}
      {contents.source === 'mod' && (
        <p className="text-sm text-muted-foreground">
          Showing the mod's own file, as the instance has no copy yet. Saving a key makes the instance's copy from it.
        </p>
      )}
      {contents.source === 'nothing' && (
        <p className="text-sm text-muted-foreground">This file does not exist yet. Saving a key creates it.</p>
      )}
      {contents.entries.length === 0 && contents.source !== 'nothing' && (
        <p className="text-sm text-muted-foreground">This file has no keys yet.</p>
      )}
      {groupBySection(contents.entries).map((group) => (
        <div key={group.section} className="space-y-2">
          <h3 className="font-mono text-sm font-semibold">[{group.section}]</h3>
          <ul className="divide-y divide-border rounded-xl border border-border">
            {group.entries.map((entry, index) => (
              <IniKeyRow
                key={`${entry.key}-${index}`}
                entry={entry}
                disabled={busy || readOnly}
                onSave={(value) => onSaveKey(entry.section, entry.key, value)}
                onRemove={() => onRemoveKey(entry)}
              />
            ))}
          </ul>
        </div>
      ))}
      <form
        className="grid gap-2 rounded-xl border border-border bg-card p-4 md:grid-cols-[1fr_1fr_1fr_auto] md:items-end"
        onSubmit={(event) => {
          event.preventDefault();
          if (!readOnly && newSection.trim() && newKey.trim()) void addKey();
        }}
      >
        <label className="space-y-1 text-sm">
          <span className="text-muted-foreground">Section</span>
          <Input
            value={newSection}
            onChange={(event) => setNewSection(event.target.value)}
            placeholder="General"
            disabled={busy || readOnly}
          />
        </label>
        <label className="space-y-1 text-sm">
          <span className="text-muted-foreground">Key</span>
          <Input
            value={newKey}
            onChange={(event) => setNewKey(event.target.value)}
            placeholder="sLanguage"
            disabled={busy || readOnly}
          />
        </label>
        <label className="space-y-1 text-sm">
          <span className="text-muted-foreground">Value</span>
          <Input value={newValue} onChange={(event) => setNewValue(event.target.value)} disabled={busy || readOnly} />
        </label>
        <Button type="submit" disabled={busy || readOnly || !newSection.trim() || !newKey.trim()}>
          Save key
        </Button>
      </form>
    </div>
  );
}

// ---------------------------------------------------------------------------
// INI files from mods
// ---------------------------------------------------------------------------

/** Why a mod INI row's mod is not the text the user can edit right now. */
function modIniOwner(row: ModIniRow): string {
  if (row.modName === '') return 'No mod ships this file';
  if (!row.modEnabled) return `${row.modName} (turned off)`;
  if (row.shadowedBy) return `${row.modName}, with ${row.shadowedBy} after it`;
  return row.modName;
}

function ModIniSection({ instanceId }: { instanceId: string }) {
  const { confirm } = useConfirm();
  const { state, refetch, setState } = useReadOnMount(listModInis, instanceId);
  const [selected, setSelected] = useState<string | null>(null);
  const [contents, setContents] = useState<IniContents | null>(null);
  const [fileState, setFileState] = useState<'idle' | 'loading' | 'failed'>('idle');
  const [fileError, setFileError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const rows = state.status === 'loaded' ? state.value : [];
  const activePath = selected !== null && rows.some((row) => row.path === selected)
    ? selected
    : rows[0]?.path ?? null;
  const activeRow = rows.find((row) => row.path === activePath) ?? null;

  useEffect(() => {
    if (activePath === null) {
      setContents(null);
      setFileState('idle');
      return;
    }
    let cancelled = false;
    setFileState('loading');
    setFileError(null);
    getModIni(instanceId, activePath)
      .then((read) => {
        if (cancelled) return;
        setContents(read);
        setFileState('idle');
      })
      .catch((failure: unknown) => {
        if (cancelled) return;
        setFileError(describeFailure(failure));
        setFileState('failed');
      });
    return () => {
      cancelled = true;
    };
  }, [instanceId, activePath]);

  const saveKey = async (section: string, key: string, value: string | null) => {
    if (activeRow === null) return false;
    setBusy(true);
    setError(null);
    setNotice(null);
    const movedGameCopy = activeRow.gameChanged;
    try {
      const updated = await setModIniValue(instanceId, activeRow.path, section, key, value);
      setContents(updated);
      setState({
        status: 'loaded',
        value: rows.map((row) =>
          row.path === activeRow.path ? { ...row, overridden: true, gameChanged: false } : row,
        ),
      });
      const aside = movedGameCopy ? " The game's changed copy was kept aside." : '';
      if (value === null) {
        setNotice(`Removed ${key} from [${section}]. The mod's own file was not changed.${aside}`);
      } else {
        setNotice(`Saved ${key} in [${section}]. The mod's own file was not changed.${aside}`);
      }
      return true;
    } catch (failure: unknown) {
      setError(describeFailure(failure));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const removeKey = async (entry: IniEntryRow) => {
    if (activeRow === null) return;
    const confirmed = await confirm({
      title: `Remove ${entry.key} from [${entry.section}]?`,
      body: `Agora takes this key out of this instance's copy of ${activeRow.path}. The mod's own file is not changed.`,
      confirmLabel: 'Remove key',
      tone: 'danger',
    });
    if (!confirmed) return;
    await saveKey(entry.section, entry.key, null);
  };

  const keepGameVersion = async () => {
    if (activeRow === null) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const updated = await keepModIniGameVersion(instanceId, activeRow.path);
      setState({ status: 'loaded', value: updated });
      const read = await getModIni(instanceId, activeRow.path);
      setContents(read);
      setNotice("The game's version is now this instance's copy. The game's file was not changed.");
    } catch (failure: unknown) {
      setError(describeFailure(failure));
    } finally {
      setBusy(false);
    }
  };

  const goBackToMine = async () => {
    if (activeRow === null) return;
    const confirmed = await confirm({
      title: `Go back to your version of ${activeRow.path}?`,
      body: "Agora keeps the copy the game wrote aside and uses your version again.",
      confirmLabel: 'Go back to my version',
      tone: 'danger',
    });
    if (!confirmed) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const updated = await restoreModIniMine(instanceId, activeRow.path);
      setState({ status: 'loaded', value: updated });
      const read = await getModIni(instanceId, activeRow.path);
      setContents(read);
      setNotice("Your version is in use again. The game's copy was kept aside.");
    } catch (failure: unknown) {
      setError(describeFailure(failure));
    } finally {
      setBusy(false);
    }
  };

  const reset = async () => {
    if (activeRow === null) return;
    const confirmed = await confirm({
      title: `Reset ${activeRow.path}?`,
      body: "Agora removes this instance's copy and keeps any copy the game wrote aside, so the mod's own file is used again.",
      confirmLabel: "Reset to the mod's version",
      tone: 'danger',
    });
    if (!confirmed) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const updated = await resetModIni(instanceId, activeRow.path);
      setState({ status: 'loaded', value: updated });
      if (updated.some((row) => row.path === activeRow.path)) {
        const read = await getModIni(instanceId, activeRow.path);
        setContents(read);
      } else {
        setContents(null);
      }
      setNotice(`${activeRow.path} is back to the version its mod ships.`);
    } catch (failure: unknown) {
      setError(describeFailure(failure));
    } finally {
      setBusy(false);
    }
  };

  return (
    <SectionFrame title="From your mods" id="ini-saves-mod-heading">
      {state.status === 'loading' && <p className="text-sm text-muted-foreground">Reading the mods' INI files…</p>}
      {state.status === 'failed' && <FailureNote message={state.message} onRetry={refetch} />}
      {state.status === 'loaded' && rows.length === 0 && (
        <p className="text-sm text-muted-foreground">None of this instance's enabled mods ships an INI file.</p>
      )}
      {state.status === 'loaded' && rows.length > 0 && (
        <div className="space-y-4" data-testid="mod-ini-panel">
          <p className="max-w-2xl text-sm text-muted-foreground">
            Mods bring their own INI files, such as a display tweak's settings. Editing one keeps this instance's own
            copy; the mod's file is not changed, and other instances keep using it as it is.
          </p>
          <div className="flex flex-wrap gap-2" role="group" aria-label="Mod INI files">
            {rows.map((row) => {
              const isActive = row.path === activePath;
              return (
                <button
                  key={row.path}
                  type="button"
                  aria-pressed={isActive}
                  onClick={() => setSelected(row.path)}
                  className={[
                    'rounded-lg px-3 py-1.5 text-left text-sm transition-colors',
                    isActive
                      ? 'bg-primary text-primary-foreground shadow-sm'
                      : 'border border-border text-muted-foreground hover:bg-accent hover:text-accent-foreground',
                  ].join(' ')}
                >
                  <span className="font-mono">{row.path}</span>{' '}
                  <span className="text-xs opacity-80">({modIniOwner(row)})</span>{' '}
                  {row.overridden && <span className="text-xs font-semibold">Edited for this instance</span>}
                </button>
              );
            })}
          </div>

          {fileState === 'loading' && <p className="text-sm text-muted-foreground">Reading the file…</p>}
          {fileState === 'failed' && fileError && (
            <p role="alert" className="rounded-lg border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">
              {fileError}
            </p>
          )}
          {activeRow?.gameChanged && (
            <div
              role="status"
              className="space-y-3 rounded-lg border border-border bg-accent/50 p-3 text-sm"
              data-testid="mod-ini-game-changed"
            >
              <p>
                The game changed this file after your last edit (for example from an in-game settings menu). Agora uses
                the game's version.
              </p>
              <div className="flex flex-wrap gap-2">
                <Button size="sm" onClick={() => void keepGameVersion()} disabled={busy}>
                  Keep the game's version
                </Button>
                <Button size="sm" variant="outline" onClick={() => void goBackToMine()} disabled={busy}>
                  Go back to my version
                </Button>
              </div>
            </div>
          )}
          {fileState === 'idle' && contents && activeRow && (
            <IniFileEditor
              contents={contents}
              busy={busy}
              readOnly={!activeRow.modEnabled}
              note={
                activeRow.modEnabled ? undefined : (
                  <p className="text-sm text-muted-foreground">
                    {activeRow.modName === '' ? 'No mod ships this file any more' : 'Its mod is turned off'}, so it can
                    only be reset to the mod's version.
                  </p>
                )
              }
              onSaveKey={saveKey}
              onRemoveKey={removeKey}
            />
          )}
          {activeRow?.overridden && (
            <div className="flex flex-wrap gap-2">
              <Button variant="outline" onClick={() => void reset()} disabled={busy}>
                Reset to the mod's version
              </Button>
            </div>
          )}
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
    </SectionFrame>
  );
}
