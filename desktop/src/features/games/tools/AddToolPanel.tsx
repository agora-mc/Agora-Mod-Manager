/**
 * "Add a tool…": the way a player puts a program on the instance's Tools list by hand, as Mod
 * Organizer 2's executables dialog does. The programs this instance's mods ship are offered first;
 * "Browse…" picks any program on this PC. Nothing is added until the player presses "Add tool".
 */

import { useEffect, useState } from 'react';
import { FolderOpen } from 'lucide-react';
import { FormField, FORM_CONTROL_CLASS } from '@/components/instance-kit';
import { Switch } from '@/components/ui/switch';
import { cn } from '@/lib/utils';
import { addUserTool, findUserTools, pickProgram, type ToolCandidate } from './api';
import { messageOf } from './toolCopy';

interface AddToolPanelProps {
  instanceId: string;
  /** The ids of the tools the list already shows, so a found copy of one is not offered again. */
  listedToolIds: readonly string[];
  onAdded: (name: string) => void;
  onClose: () => void;
}

/** A name for a program the player picked: its file name without `.exe`. */
export function nameFromExe(exe: string): string {
  const base = exe.replace(/\\/g, '/').split('/').filter(Boolean).pop() ?? '';
  return base.replace(/\.exe$/i, '');
}

/** The program's arguments as the player typed them: spaces separate, quotes keep spaces in one. */
export function splitArguments(text: string): string[] {
  const out: string[] = [];
  let current = '';
  let quoted = false;
  let any = false;
  for (const char of text) {
    if (char === '"') {
      quoted = !quoted;
      any = true;
    } else if (/\s/.test(char) && !quoted) {
      if (any) out.push(current);
      current = '';
      any = false;
    } else {
      current += char;
      any = true;
    }
  }
  if (any) out.push(current);
  return out;
}

export function AddToolPanel({ instanceId, listedToolIds, onAdded, onClose }: AddToolPanelProps) {
  const [found, setFound] = useState<ToolCandidate[] | null>(null);
  const [findError, setFindError] = useState<string | null>(null);
  const [exe, setExe] = useState('');
  const [name, setName] = useState('');
  // The name follows the chosen program until the player types one of their own.
  const [nameTyped, setNameTyped] = useState(false);
  const [args, setArgs] = useState('');
  const [isMod, setIsMod] = useState(false);
  const [folder, setFolder] = useState('');
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let current = true;
    findUserTools(instanceId)
      .then((candidates) => { if (current) setFound(candidates); })
      .catch((reason: unknown) => { if (current) { setFound([]); setFindError(messageOf(reason)); } });
    return () => { current = false; };
  }, [instanceId]);

  // A program already on the list is not offered: neither one added before, nor a copy of a tool
  // the game recognises that the list already shows.
  const offered = (found ?? []).filter((candidate) => !candidate.alreadyAdded
    && !(candidate.knownToolId && listedToolIds.includes(candidate.knownToolId)));

  const choose = (path: string, suggested: string) => {
    setExe(path);
    if (!nameTyped || name.trim() === '') setName(suggested);
    setError(null);
  };

  const browse = async () => {
    setError(null);
    try {
      const picked = await pickProgram();
      if (picked) choose(picked, nameFromExe(picked));
    } catch (reason) {
      setError(messageOf(reason));
    }
  };

  const add = async () => {
    setError(null);
    if (!exe.trim()) {
      setError('Choose a program first.');
      return;
    }
    if (!name.trim()) {
      setError('Give the tool a name.');
      return;
    }
    if (isMod && !folder.trim()) {
      setError('A tool whose output is a mod needs an output folder.');
      return;
    }
    setAdding(true);
    try {
      const added = await addUserTool(instanceId, {
        name: name.trim(),
        exe: exe.trim(),
        args: splitArguments(args),
        outputIsMod: isMod,
        outputFolder: isMod ? folder.trim() : null,
      });
      onAdded(added.name);
    } catch (reason) {
      setError(messageOf(reason));
    } finally {
      setAdding(false);
    }
  };

  return (
    <section
      aria-labelledby="tool-add-heading"
      className="space-y-4 rounded-xl border border-primary/30 bg-primary/5 p-4"
      data-testid="tool-add"
    >
      <div>
        <h3 id="tool-add-heading" className="text-sm font-semibold">Add a tool</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          Pick a program from this instance’s mods, or browse for any program on this PC.
        </p>
      </div>

      <div className="space-y-2">
        <p className="text-sm font-medium">Found in your mods</p>
        {found === null && <p className="text-sm text-muted-foreground">Looking through your mods…</p>}
        {findError && <p role="alert" className="text-sm text-destructive">Your mods could not be searched: {findError}</p>}
        {found !== null && !findError && offered.length === 0 && (
          <p className="text-sm text-muted-foreground" data-testid="tool-add-none-found">No other programs in this instance’s mods.</p>
        )}
        {offered.length > 0 && (
          <div className="flex flex-wrap gap-2" data-testid="tool-add-found">
            {offered.map((candidate) => {
              const chosen = exe === candidate.exe;
              return (
                <button
                  key={candidate.exe}
                  type="button"
                  aria-pressed={chosen}
                  onClick={() => choose(candidate.exe, candidate.suggestedName)}
                  title={candidate.exe}
                  className={cn(
                    'rounded-lg border px-3 py-1.5 text-left text-sm hover:bg-accent',
                    chosen ? 'border-primary bg-primary/15 font-medium' : 'border-border bg-card',
                  )}
                >
                  <span className="block">{candidate.suggestedName}</span>
                  {candidate.modName && <span className="block text-xs text-muted-foreground">from {candidate.modName}</span>}
                </button>
              );
            })}
          </div>
        )}
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <FormField label="Program" className="sm:col-span-2" hint="A program a mod puts in the game folder, or one anywhere on this PC.">
          {(control) => (
            <div className="flex gap-2">
              <input
                {...control}
                type="text"
                value={exe}
                placeholder="Data/tools/My Tool.exe"
                onChange={(event) => setExe(event.target.value)}
                className={FORM_CONTROL_CLASS}
              />
              <button
                type="button"
                onClick={() => void browse()}
                className="mt-1 inline-flex shrink-0 items-center gap-2 rounded-lg border border-border bg-card px-3 py-2 text-sm font-medium hover:bg-accent"
              >
                <FolderOpen className="h-4 w-4" aria-hidden="true" />
                Browse…
              </button>
            </div>
          )}
        </FormField>
        <FormField label="Name">
          {(control) => (
            <input
              {...control}
              type="text"
              value={name}
              onChange={(event) => { setName(event.target.value); setNameTyped(true); }}
              className={FORM_CONTROL_CLASS}
            />
          )}
        </FormField>
        <FormField label="Arguments (optional)">
          {(control) => (
            <input {...control} type="text" value={args} onChange={(event) => setArgs(event.target.value)} className={FORM_CONTROL_CLASS} />
          )}
        </FormField>
        <div className="flex items-center gap-2 text-sm sm:col-span-2">
          <Switch aria-label="Its output is a mod" checked={isMod} onCheckedChange={setIsMod} />
          Its output is a mod
        </div>
        {isMod && (
          <FormField
            label="Output folder"
            className="sm:col-span-2"
            hint="The folder you set in the tool’s own settings, next to the program or a full path. It must be outside the game folder."
          >
            {(control) => (
              <input
                {...control}
                type="text"
                value={folder}
                placeholder="MyTool_Output"
                onChange={(event) => setFolder(event.target.value)}
                className={FORM_CONTROL_CLASS}
              />
            )}
          </FormField>
        )}
      </div>

      {error && <p role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">{error}</p>}

      <div className="flex flex-wrap gap-2">
        <button
          type="button"
          disabled={adding}
          onClick={() => void add()}
          className="rounded-lg bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
        >
          {adding ? 'Adding…' : 'Add tool'}
        </button>
        <button
          type="button"
          onClick={onClose}
          className="rounded-lg border border-border bg-card px-4 py-2 text-sm font-medium hover:bg-accent"
        >
          Cancel
        </button>
      </div>
    </section>
  );
}
