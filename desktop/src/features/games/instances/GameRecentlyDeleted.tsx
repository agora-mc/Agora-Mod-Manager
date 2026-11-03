/**
 * Recently deleted under a game's instance cards: Minecraft's list (the kit's panel) with this
 * game's deleted instances, plus Empty. Restore asks for a new name when another instance has
 * taken the deleted one's place.
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import { RecentlyDeletedPanel, type RecentlyDeletedItem } from '@/components/instance-kit';
import { InstanceIcon } from '@/components/InstanceIcon';
import { useConfirm } from '@/components/ui/confirm';
import { instanceFamilyHue } from '@/lib/instanceIdentity';
import {
  gamesErrorCode,
  gamesErrorMessage,
  listDeletedGameInstances,
  purgeDeletedGameInstances,
  restoreDeletedGameInstance,
  type DeletedEntry,
  type PurgedDeleted,
} from './api';

/** The window Agora uses when nobody changed it, until the list says otherwise. */
export const DEFAULT_RETENTION_DAYS = 30;

function purgeProblem(result: PurgedDeleted): string | null {
  if (result.failed === 0) return null;
  return result.failed === 1
    ? 'One instance could not be moved to the Recycle Bin. It is still here.'
    : `${result.failed} instances could not be moved to the Recycle Bin. They are still here.`;
}

export function GameRecentlyDeleted({
  gameId,
  refreshKey,
  onRestored,
  onRetentionDays,
}: {
  gameId: string;
  /** Changes when the instance list does, so a fresh delete shows up here. */
  refreshKey: unknown;
  onRestored: () => void;
  /** The player's window, for the Delete question on the cards. */
  onRetentionDays?: (days: number) => void;
}) {
  const { confirm, prompt } = useConfirm();
  const [entries, setEntries] = useState<DeletedEntry[]>([]);
  const [retentionDays, setRetentionDays] = useState(DEFAULT_RETENTION_DAYS);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const reportDays = useRef(onRetentionDays);
  reportDays.current = onRetentionDays;

  const reload = useCallback(async () => {
    try {
      const list = await listDeletedGameInstances(gameId);
      setEntries(list.entries);
      setRetentionDays(list.retentionDays);
      reportDays.current?.(list.retentionDays);
    } catch (failure) {
      setError(`Recently deleted could not be read. ${gamesErrorMessage(failure)}`);
    }
  }, [gameId]);

  useEffect(() => {
    void reload();
  }, [reload, refreshKey]);

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await action();
    } finally {
      setBusy(false);
    }
  };

  const restore = (entry: DeletedEntry) => run(async () => {
    try {
      await restoreDeletedGameInstance(entry.entryId);
      setNotice(`Restored ${entry.name}.`);
    } catch (failure) {
      if (gamesErrorCode(failure) !== 'instance_id_taken') {
        setError(`${entry.name} was not restored. ${gamesErrorMessage(failure)}`);
        return;
      }
      const name = await prompt({
        title: `Restore ${entry.name} under a new name`,
        body: 'Another instance has taken its place since it was deleted. Pick a name for the restored one.',
        initialValue: `${entry.name} (restored)`,
        confirmLabel: 'Restore',
      });
      if (name === null || name.trim() === '') return;
      try {
        await restoreDeletedGameInstance(entry.entryId, name.trim());
        setNotice(`Restored ${entry.name} as "${name.trim()}".`);
      } catch (again) {
        setError(`${entry.name} was not restored. ${gamesErrorMessage(again)}`);
        return;
      }
    }
    onRestored();
    await reload();
  });

  const deleteNow = async (entry: DeletedEntry) => {
    if (!await confirm({
      title: `Delete "${entry.name}" now?`,
      body: "This sends Agora's copy to your Recycle Bin straight away. After that you can only get it back from there, not from Agora.",
      confirmLabel: 'Delete now',
      tone: 'danger',
    })) return;
    await run(async () => {
      try {
        const result = await purgeDeletedGameInstances(gameId, entry.entryId);
        setError(purgeProblem(result));
      } catch (failure) {
        setError(`${entry.name} was not deleted. ${gamesErrorMessage(failure)}`);
      }
      await reload();
    });
  };

  const empty = async () => {
    const count = entries.length;
    if (!await confirm({
      title: 'Empty Recently deleted?',
      body: `This sends ${count === 1 ? 'the deleted instance' : `all ${count} deleted instances`} of this game to your Recycle Bin straight away. After that you can only get ${count === 1 ? 'it' : 'them'} back from there, not from Agora.`,
      confirmLabel: 'Empty',
      tone: 'danger',
    })) return;
    await run(async () => {
      try {
        const result = await purgeDeletedGameInstances(gameId);
        setError(purgeProblem(result));
      } catch (failure) {
        setError(`Recently deleted was not emptied. ${gamesErrorMessage(failure)}`);
      }
      await reload();
    });
  };

  const byId = (item: RecentlyDeletedItem) => entries.find((entry) => entry.entryId === item.id);

  return (
    <RecentlyDeletedPanel
      entries={entries.map((entry) => ({
        id: entry.entryId,
        name: entry.name,
        deletedAt: entry.deletedAt,
        expiresAt: entry.expiresAt,
        sizeBytes: entry.sizeBytes,
      }))}
      retentionDays={retentionDays}
      busy={busy}
      notice={notice}
      error={error}
      onRestore={(item) => {
        const entry = byId(item);
        if (entry) void restore(entry);
      }}
      onDeleteNow={(item) => {
        const entry = byId(item);
        if (entry) void deleteNow(entry);
      }}
      onEmpty={() => void empty()}
      tile={(item) => {
        const entry = byId(item);
        return (
          <InstanceIcon
            name={item.name}
            seed={entry?.instanceId ?? item.id}
            familyHue={instanceFamilyHue(entry?.instanceId ?? item.id)}
            size={36}
            className="opacity-70 grayscale-[35%]"
          />
        );
      }}
    />
  );
}
