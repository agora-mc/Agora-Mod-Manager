/**
 * "Scan for programs": lists the programs in this instance's enabled mods and its own files, each
 * with the mod it came from, for the player to pick from. The same search "Add a tool…" offers from
 * (`findUserTools`). Nothing is added until the player presses the button, and a search that finds
 * nothing says so.
 */

import { useCallback, useEffect, useState } from 'react';
import { ScanSearch } from 'lucide-react';
import { addUserTool, findUserTools, type ToolCandidate } from './api';
import { candidateOrigin, messageOf, offeredCandidates } from './toolCopy';

interface ToolScanPanelProps {
  instanceId: string;
  /** The ids of the tools the list already shows, so a found copy of one is not offered again. */
  listedToolIds: readonly string[];
  /** Tools were added. `keepOpen`: some of what was picked could not be, and the panel says why. */
  onAdded: (names: string[], keepOpen: boolean) => void;
  onClose: () => void;
}

const BUTTON = 'rounded-lg border border-border bg-card px-4 py-2 text-sm font-medium hover:bg-accent disabled:opacity-50';

/**
 * What the scan offers: nothing on the list already, and none of the game's own programs. Sorted by
 * name, since the player reads this as a list of programs and not of paths.
 */
export function scanOffers(found: readonly ToolCandidate[], listedToolIds: readonly string[]): ToolCandidate[] {
  return offeredCandidates(found, listedToolIds)
    .filter((candidate) => candidate.source !== 'game')
    .sort((a, b) => a.suggestedName.localeCompare(b.suggestedName, undefined, { sensitivity: 'base', numeric: true }));
}

export function ToolScanPanel({ instanceId, listedToolIds, onAdded, onClose }: ToolScanPanelProps) {
  const [found, setFound] = useState<ToolCandidate[] | null>(null);
  const [scanError, setScanError] = useState<string | null>(null);
  const [picked, setPicked] = useState<ReadonlySet<string>>(new Set());
  const [adding, setAdding] = useState(false);
  const [failures, setFailures] = useState<string[]>([]);

  const scan = useCallback(async (isCurrent: () => boolean) => {
    setFound(null);
    setScanError(null);
    try {
      const candidates = await findUserTools(instanceId);
      if (isCurrent()) setFound(candidates);
    } catch (reason) {
      if (isCurrent()) setScanError(messageOf(reason));
    }
  }, [instanceId]);

  useEffect(() => {
    let current = true;
    void scan(() => current);
    return () => { current = false; };
  }, [scan]);

  const offers = scanOffers(found ?? [], listedToolIds);
  const allPicked = offers.length > 0 && offers.every((candidate) => picked.has(candidate.exe));

  const toggle = (exe: string) => {
    setPicked((current) => {
      const next = new Set(current);
      if (next.has(exe)) next.delete(exe);
      else next.add(exe);
      return next;
    });
  };

  const add = async () => {
    setAdding(true);
    setFailures([]);
    const added: string[] = [];
    const failed: string[] = [];
    const addedExes = new Set<string>();
    // One at a time: each add rewrites the instance's list, under its lock.
    for (const candidate of offers.filter((offer) => picked.has(offer.exe))) {
      try {
        const tool = await addUserTool(instanceId, {
          name: candidate.suggestedName,
          exe: candidate.exe,
          args: [],
          outputIsMod: false,
          outputFolder: null,
          keepsOutput: false,
        });
        added.push(tool.name);
        addedExes.add(candidate.exe);
      } catch (reason) {
        failed.push(`${candidate.suggestedName}: ${messageOf(reason)}`);
      }
    }
    setAdding(false);
    setFailures(failed);
    if (added.length > 0) {
      setFound((current) => (current ?? []).filter((candidate) => !addedExes.has(candidate.exe)));
      setPicked((current) => new Set([...current].filter((exe) => !addedExes.has(exe))));
      onAdded(added, failed.length > 0);
    }
  };

  return (
    <section
      aria-labelledby="tool-scan-heading"
      className="space-y-4 rounded-xl border border-primary/30 bg-primary/5 p-4"
      data-testid="tool-scan"
    >
      <div className="flex items-start gap-3">
        <span className="settings-card-icon" aria-hidden="true">
          <ScanSearch className="h-4 w-4" />
        </span>
        <div className="min-w-0">
          <h3 id="tool-scan-heading" className="text-sm font-semibold">Scan for programs</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            Looks in this instance’s enabled mods and its own files. The game’s own programs and tools already on your
            list are left out. Pick the ones you want; nothing is added until you say so.
          </p>
        </div>
      </div>

      {found === null && !scanError && (
        <p role="status" className="text-sm text-muted-foreground" data-testid="tool-scan-busy">Looking through your mods…</p>
      )}

      {scanError && (
        <div role="alert" className="space-y-2 rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">
          <p>Your mods could not be searched: {scanError}</p>
          <button type="button" className={BUTTON} onClick={() => void scan(() => true)}>Try again</button>
        </div>
      )}

      {found !== null && offers.length === 0 && (
        <p role="status" className="rounded-lg border border-dashed border-border p-3 text-sm" data-testid="tool-scan-none">
          No new programs found in this instance’s enabled mods or its own files. A program in a mod that is turned off
          isn’t searched.
        </p>
      )}

      {offers.length > 0 && (
        <div className="space-y-2">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p role="status" className="text-sm font-medium" data-testid="tool-scan-count">
              Found {offers.length === 1 ? '1 program' : `${offers.length} programs`}
            </p>
            {offers.length > 1 && (
              <button
                type="button"
                className="text-xs font-medium text-primary hover:underline"
                onClick={() => setPicked(allPicked ? new Set() : new Set(offers.map((candidate) => candidate.exe)))}
              >
                {allPicked ? 'Clear all' : 'Select all'}
              </button>
            )}
          </div>
          <ul className="max-h-80 space-y-2 overflow-y-auto pr-1" aria-label="Programs found" data-testid="tool-scan-list">
            {offers.map((candidate) => {
              const origin = candidateOrigin(candidate);
              return (
                <li key={candidate.exe}>
                  <label className={[
                    'flex cursor-pointer items-start gap-3 rounded-lg border p-3 hover:bg-accent/50',
                    picked.has(candidate.exe) ? 'border-primary bg-primary/10' : 'border-border bg-card',
                  ].join(' ')}>
                    <input
                      type="checkbox"
                      className="mt-1 accent-[hsl(var(--primary))]"
                      checked={picked.has(candidate.exe)}
                      onChange={() => toggle(candidate.exe)}
                    />
                    <span className="min-w-0 flex-1">
                      <span className="block text-sm font-medium">{candidate.suggestedName}</span>
                      {origin && <span className="block text-xs text-muted-foreground">{origin}</span>}
                      <span className="block break-all font-mono text-xs text-muted-foreground">{candidate.exe}</span>
                    </span>
                  </label>
                </li>
              );
            })}
          </ul>
        </div>
      )}

      {failures.length > 0 && (
        <div role="alert" className="rounded-lg border border-destructive bg-destructive/10 p-3 text-sm">
          <p>These could not be added:</p>
          <ul className="mt-1 list-disc pl-5">
            {failures.map((line) => <li key={line}>{line}</li>)}
          </ul>
        </div>
      )}

      <div className="flex flex-wrap gap-2">
        {offers.length > 0 && (
          <button
            type="button"
            disabled={adding || picked.size === 0}
            onClick={() => void add()}
            className="rounded-lg bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            {adding ? 'Adding…' : picked.size === 0 ? 'Add programs' : picked.size === 1 ? 'Add 1 program' : `Add ${picked.size} programs`}
          </button>
        )}
        <button type="button" className={BUTTON} disabled={adding} onClick={onClose}>
          Close
        </button>
      </div>
    </section>
  );
}
