/**
 * The copy of the game's Data folder an instance keeps for swap runs, and a way to delete it:
 * the CLI's `games tools-swap remove-copy`. Shown only when such a copy exists. Deleting it frees
 * the space; the next swap run copies Data again.
 */

import { useCallback, useEffect, useState } from 'react';
import { useConfirm } from '@/components/ui/confirm';
import { formatBytes } from '../content/api';
import { gameToolSwapCosts, removeKeptSwapCopy, type SwapCosts } from './api';

export function KeptCopySection({ instanceId, disabled }: { instanceId: string; disabled: boolean }) {
  const { confirm } = useConfirm();
  const [costs, setCosts] = useState<SwapCosts | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async (isCurrent: () => boolean) => {
    try {
      const value = await gameToolSwapCosts(instanceId);
      if (isCurrent()) setCosts(value);
    } catch {
      // A game with no swap support has no kept copy to show.
      if (isCurrent()) setCosts(null);
    }
  }, [instanceId]);

  useEffect(() => {
    let current = true;
    void load(() => current);
    return () => { current = false; };
  }, [load]);

  if (!costs || costs.copyKind !== 'kept' || !costs.keptCopyExists) {
    return note ? <p role="status" className="text-sm text-muted-foreground">{note}</p> : null;
  }

  const remove = async () => {
    const ok = await confirm({
      title: 'Delete the kept copy of Data?',
      body: `${costs.copyFolder}\n\nThe game's own Data folder is not touched. The next tool run that swaps Data copies it again.`,
      confirmLabel: 'Delete the copy',
      tone: 'danger',
    });
    if (!ok) return;
    setBusy(true);
    try {
      const removed = await removeKeptSwapCopy(instanceId);
      setNote(removed ? 'The kept copy of Data was deleted.' : 'There was no kept copy to delete.');
      await load(() => true);
    } catch (error) {
      setNote(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="space-y-2 rounded-xl border border-border p-4 text-sm" data-testid="kept-swap-copy">
      <p>
        A copy of the game&apos;s Data folder ({costs.copyFiles} files, {formatBytes(costs.copyBytes)}) is kept so tools
        that work on the real install can run on it.
      </p>
      <button
        type="button"
        disabled={busy || disabled}
        onClick={() => void remove()}
        className="rounded-lg border border-input px-3 py-1.5 font-medium hover:bg-accent disabled:opacity-50"
      >
        Delete the kept copy
      </button>
      {note && <p role="status" className="text-muted-foreground">{note}</p>}
    </section>
  );
}
