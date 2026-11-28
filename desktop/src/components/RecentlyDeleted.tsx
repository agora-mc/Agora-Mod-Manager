import { useEffect, useState } from 'react';
import {
  formatError,
  listRecentlyDeleted,
  purgeDeletedInstance,
  restoreDeletedInstance,
  type RecentlyDeletedEntry,
} from '../lib/tauri';
import { useConfirm } from '@/components/ui/confirm';
import { RecentlyDeletedPanel, daysText } from '@/components/instance-kit';
import { importSourceLabel, isFileImportSource } from '../lib/importDisplay';
import { RECOMMENDED_RETENTION_DAYS, useRecentlyDeletedDays } from '../lib/recentlyDeletedDays';

function launcherName(source: string | null | undefined): string | null {
  if (!source) return null;
  if (isFileImportSource(source)) return importSourceLabel(source);
  if (source === 'curse_forge' || source === 'curseforge') return 'CurseForge';
  if (source === 'prism') return 'Prism Launcher';
  return 'Modrinth App';
}

/** Say exactly what Delete does: Agora's copy only, restorable for the player's window. */
export function deleteDialogBody(
  instance: { name: string; import_source?: string | null },
  retentionDays: number = RECOMMENDED_RETENTION_DAYS,
): string {
  const launcher = launcherName(instance.import_source);
  const untouched = isFileImportSource(instance.import_source)
    ? ` The pack file ${launcher} isn't touched.`
    : launcher ? ` Your ${launcher} files aren't touched.` : '';
  return `Removes Agora's copy of ${instance.name}.${untouched} You can restore it from Recently deleted for ${daysText(retentionDays)}.`;
}

/**
 * Instances deleted within the player's window (the setting every game shares), with Restore
 * and Delete now.
 * Renders nothing while the list is empty. `refreshKey` changes when the
 * instance list does, so a fresh delete shows up without a manual reload.
 */
export function RecentlyDeleted({
  refreshKey,
  onRestored,
}: {
  refreshKey: unknown;
  onRestored: () => void;
}) {
  const { confirm } = useConfirm();
  const retentionDays = useRecentlyDeletedDays();
  const [entries, setEntries] = useState<RecentlyDeletedEntry[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const reload = async () => {
    try {
      const rows = await listRecentlyDeleted();
      setEntries(Array.isArray(rows) ? rows : []);
    } catch (cause) {
      setError(formatError(cause));
    }
  };

  useEffect(() => {
    void reload();
  }, [refreshKey]);

  const restore = async (entry: RecentlyDeletedEntry) => {
    setBusy(entry.entry_id);
    setError(null);
    setNotice(null);
    try {
      const outcome = await restoreDeletedInstance(entry.entry_id);
      setNotice(
        outcome.renamed
          ? `Restored ${entry.name} as "${entry.name} (restored)" because "${outcome.original_instance_id}" is in use by another instance.`
          : `Restored ${entry.name}.`,
      );
      onRestored();
      await reload();
    } catch (cause) {
      setError(formatError(cause));
    } finally {
      setBusy(null);
    }
  };

  const deleteNow = async (entry: RecentlyDeletedEntry) => {
    if (!await confirm({
      title: `Delete "${entry.name}" now?`,
      body: "This sends Agora's copy to your Recycle Bin straight away. After that you can only get it back from there, not from Agora.",
      confirmLabel: 'Delete now',
      tone: 'danger',
    })) return;
    setBusy(entry.entry_id);
    setError(null);
    setNotice(null);
    try {
      await purgeDeletedInstance(entry.entry_id);
      await reload();
    } catch (cause) {
      setError(formatError(cause));
    } finally {
      setBusy(null);
    }
  };

  return (
    <RecentlyDeletedPanel
      entries={entries.map((entry) => ({
        id: entry.entry_id,
        name: entry.name,
        deletedAt: entry.deleted_at,
        expiresAt: entry.expires_at,
        sizeBytes: entry.size_bytes,
      }))}
      retentionDays={retentionDays}
      busy={busy !== null}
      notice={notice}
      error={error}
      onRestore={(item) => {
        const entry = entries.find((row) => row.entry_id === item.id);
        if (entry) void restore(entry);
      }}
      onDeleteNow={(item) => {
        const entry = entries.find((row) => row.entry_id === item.id);
        if (entry) void deleteNow(entry);
      }}
    />
  );
}
