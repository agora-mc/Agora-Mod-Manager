/**
 * A game instance's recovery points. The same snapshot store, LKG rules and
 * guarded restore as a Minecraft instance, with the sizes that matter for a
 * setup whose mods can be tens of GB.
 */

import { useCallback, useEffect, useState } from 'react';
import { useConfirm } from '@/components/ui/confirm';
import {
  createSnapshot,
  deleteSnapshot,
  describeCollection,
  describeRestoreCoverage,
  detectDrift,
  errorCode,
  exportBackup,
  gameBackupBytes,
  importBackup,
  listSnapshots,
  pickDirectory,
  pickOpenFile,
  previewBackupImport,
  restoreSnapshot,
  type Snapshot,
  type SnapshotDiff,
} from '@/lib/tauri';

function formatBytes(bytes: number): string {
  if (bytes < 1_000) return `${bytes} B`;
  if (bytes < 1_000_000) return `${(bytes / 1_000).toFixed(1)} kB`;
  if (bytes < 1_000_000_000) return `${(bytes / 1_000_000).toFixed(1)} MB`;
  return `${(bytes / 1_000_000_000).toFixed(1)} GB`;
}

function formatError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

interface InstanceRecoveryTabProps {
  instanceId: string;
}

export function InstanceRecoveryTab({ instanceId }: InstanceRecoveryTabProps) {
  const { confirm } = useConfirm();
  const [snapshots, setSnapshots] = useState<Snapshot[]>([]);
  const [label, setLabel] = useState('');
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);
  const [diff, setDiff] = useState<{ snapshotId: string; diff: SnapshotDiff } | null>(null);

  const reload = useCallback(async () => {
    const next = await listSnapshots(instanceId);
    setSnapshots(next);
    return next;
  }, [instanceId]);

  useEffect(() => {
    let cancelled = false;
    listSnapshots(instanceId)
      .then((next) => {
        if (!cancelled) setSnapshots(next);
      })
      .catch((reason) => {
        if (!cancelled) setError(formatError(reason));
      });
    return () => {
      cancelled = true;
    };
  }, [instanceId]);

  const currentLkg = snapshots.find((snapshot) => snapshot.is_current_lkg);

  async function run<T>(key: string, action: () => Promise<T>): Promise<T | null> {
    setBusy(key);
    setError(null);
    setStatus(null);
    try {
      return await action();
    } catch (reason) {
      setError(formatError(reason));
      return null;
    } finally {
      setBusy(null);
    }
  }

  async function restore(snapshotId: string, force = false): Promise<boolean> {
    try {
      const outcome = await restoreSnapshot(instanceId, snapshotId, force);
      setDiff(null);
      await reload();
      // The restore reports a folder it captured but the game no longer declares, so the tab says
      // what came back rather than leaving the person to wonder (MASTER_SPEC §32.7).
      setStatus(
        [
          'Snapshot restored. The previous state was saved as an undo snapshot.',
          describeRestoreCoverage(outcome),
        ]
          .filter(Boolean)
          .join(' '),
      );
      return true;
    } catch (reason) {
      if (!force && errorCode(reason) === 'ERR_SNAPSHOT_NOT_IDLE') {
        const yes = await confirm({
          title: 'The game is running',
          body: `${formatError(reason)}\n\nRestoring while the game runs can be overwritten by the game. Restore anyway?`,
          confirmLabel: 'Restore anyway',
        });
        if (yes) return restore(snapshotId, true);
      }
      setError(formatError(reason));
      return false;
    }
  }

  async function exportOne(snapshot: Snapshot) {
    const bytes = await run('size', () => gameBackupBytes(instanceId, snapshot.id));
    if (bytes === null) return;
    const includeBytes = await confirm({
      title: 'Include the mod files in this backup?',
      body:
        `The backup always holds the instance's state and the hash and source of each item. ` +
        `Including the files adds ${formatBytes(bytes)}.\n\n` +
        'Without them, importing on another machine needs those items in the content store, or Agora names the missing ones.',
      confirmLabel: 'Include the files',
      cancelLabel: 'State only',
    });
    const dir = await pickDirectory('Choose a folder for this backup');
    if (!dir) return;
    const path = await run('export', () => exportBackup(instanceId, snapshot.id, dir, includeBytes));
    if (path) {
      setStatus(
        includeBytes
          ? `Backup with the mod files written to ${path}`
          : `Backup state written to ${path}`,
      );
    }
  }

  async function importOne() {
    const artifact = await pickOpenFile('Choose a backup file', ['zip']);
    if (!artifact) return;
    const preview = await run('preview', () => previewBackupImport(instanceId, artifact));
    if (!preview) return;
    let fetchMissing = false;
    if (preview.missingItems.length > 0) {
      fetchMissing = await confirm({
        title: 'This backup needs content that is not here',
        body:
          `It names ${preview.missingItems.length} item(s), ${preview.missingObjects} object(s), ` +
          `${formatBytes(preview.missingBytes)} in total:\n\n${preview.missingItems.join(', ')}\n\n` +
          'Fetch them through the catalog? Only files whose hash matches the backup are accepted. ' +
          'Nothing is applied if any of them cannot be fetched.',
        confirmLabel: 'Fetch and import',
        cancelLabel: 'Do not import',
      });
      if (!fetchMissing) {
        setStatus('Nothing imported. The backup needs content that is not in the store.');
        return;
      }
    }
    const imported = await run('import', () =>
      importBackup(instanceId, artifact, fetchMissing),
    );
    if (!imported) return;
    await reload();
    const yes = await confirm({
      title: 'Restore this backup into the instance now?',
      body:
        'The backup was checked and imported as a restorable snapshot. ' +
        'Restoring replaces the current state and takes an undo snapshot first.',
      confirmLabel: 'Restore',
      tone: 'danger',
    });
    if (yes) await restore(imported.id);
    else setStatus('Backup imported as a restorable snapshot. Nothing in the instance changed.');
  }

  return (
    <section className="rounded-xl border border-border bg-card p-4 space-y-4" aria-label="Recovery">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h3 className="font-semibold text-sm">Snapshots</h3>
          <p className="text-xs text-muted-foreground">
            A game instance's snapshot holds its manifest, plugin lists, INI copies, loadouts, save
            choice and load-after rules; mod files stay once in the content store.
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            onClick={() => void importOne()}
            disabled={busy !== null}
            className="rounded-lg border border-input bg-background px-3 py-1.5 text-sm font-medium hover:bg-accent disabled:opacity-50"
          >
            Import backup…
          </button>
          <input
            type="text"
            value={label}
            onChange={(event) => setLabel(event.target.value)}
            placeholder="Optional label…"
            className="w-44 rounded-lg border border-input bg-background px-3 py-1.5 text-sm"
          />
          <button
            type="button"
            disabled={busy !== null}
            onClick={() =>
              void run('create', async () => {
                await createSnapshot(
                  instanceId,
                  label.trim() || `Snapshot ${new Date().toLocaleString()}`,
                );
                setLabel('');
                await reload();
                setStatus('Snapshot created.');
              })
            }
            className="rounded-lg bg-primary px-4 py-1.5 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            Create snapshot
          </button>
        </div>
      </div>

      {currentLkg && (
        <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-green-500/40 bg-green-500/5 px-3 py-2">
          <p className="text-sm">
            <span className="font-medium">Last setup that launched fine:</span>{' '}
            {currentLkg.label ?? currentLkg.id}
          </p>
          <div className="flex gap-2">
            <button
              type="button"
              disabled={busy !== null}
              onClick={() =>
                void run('diff', async () => {
                  setDiff({
                    snapshotId: currentLkg.id,
                    diff: await detectDrift(instanceId, currentLkg.id),
                  });
                })
              }
              className="text-xs text-primary hover:underline disabled:opacity-50"
            >
              Changes since then
            </button>
            <button
              type="button"
              disabled={busy !== null}
              onClick={() => void run('restore-lkg', () => restore(currentLkg.id))}
              className="text-xs font-medium text-primary hover:underline disabled:opacity-50"
            >
              Restore to it
            </button>
          </div>
        </div>
      )}

      {diff && (
        <div className="rounded-lg border border-border p-3 text-xs">
          <p className="mb-1 font-medium">
            {diff.diff.added.length} added, {diff.diff.removed.length} removed,{' '}
            {diff.diff.modified.length} modified, {diff.diff.unchangedCount} unchanged
          </p>
          <ul className="space-y-0.5 font-mono">
            {[
              ...diff.diff.added.map((entry) => `+ ${entry.path}`),
              ...diff.diff.removed.map((entry) => `- ${entry.path}`),
              ...diff.diff.modified.map((entry) => `~ ${entry.path}`),
            ].map((line) => (
              <li key={line}>{line}</li>
            ))}
          </ul>
        </div>
      )}

      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
      {status && (
        <p role="status" className="text-sm text-muted-foreground">
          {status}
        </p>
      )}

      {snapshots.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          No snapshots yet. Launch the game once, or create one now.
        </p>
      ) : (
        <ul className="space-y-2">
          {snapshots.map((snapshot) => (
            <li
              key={snapshot.id}
              className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-border px-3 py-2 text-sm"
            >
              <div className="min-w-0">
                <span className="flex flex-wrap items-center gap-2 font-medium">
                  <span>{snapshot.unreadable ? 'Unreadable snapshot' : snapshot.label ?? snapshot.id}</span>
                  {snapshot.is_current_lkg && (
                    <span className="rounded-full bg-green-500/10 px-2 py-0.5 text-xs text-green-700 dark:text-green-300">
                      Last setup that launched fine
                    </span>
                  )}
                  {!snapshot.is_current_lkg && snapshot.is_lkg && (
                    <span className="rounded-full bg-primary/10 px-2 py-0.5 text-xs text-primary">
                      Launched fine before
                    </span>
                  )}
                </span>
                <span className="text-xs text-muted-foreground">
                  {new Date(snapshot.created_at).toLocaleString()} · {snapshot.file_count} file
                  {snapshot.file_count === 1 ? '' : 's'}
                </span>
                {snapshot.unreadable && (
                  <span className="block text-xs text-destructive">{snapshot.unreadable}</span>
                )}
              </div>
              <div className="flex gap-2">
                <button
                  type="button"
                  disabled={Boolean(snapshot.unreadable) || busy !== null}
                  onClick={() =>
                    void run('diff', async () => {
                      setDiff({
                        snapshotId: snapshot.id,
                        diff: await detectDrift(instanceId, snapshot.id),
                      });
                    })
                  }
                  className="text-xs text-primary hover:underline disabled:opacity-50"
                >
                  Show diff
                </button>
                <button
                  type="button"
                  disabled={Boolean(snapshot.unreadable) || busy !== null}
                  onClick={() => void run(`restore-${snapshot.id}`, () => restore(snapshot.id))}
                  className="text-xs hover:underline disabled:opacity-50"
                >
                  Restore
                </button>
                <button
                  type="button"
                  disabled={Boolean(snapshot.unreadable) || busy !== null}
                  onClick={() => void run('export', () => exportOne(snapshot))}
                  className="text-xs hover:underline disabled:opacity-50"
                >
                  Export…
                </button>
                <button
                  type="button"
                  disabled={busy !== null}
                  onClick={() =>
                    void run(`delete-${snapshot.id}`, async () => {
                      const yes = await confirm({
                        title: 'Delete this snapshot?',
                        body: 'The recovery point is removed. This cannot be undone.',
                        confirmLabel: 'Delete',
                        tone: 'danger',
                      });
                      if (!yes) return;
                      const collected = await deleteSnapshot(instanceId, snapshot.id);
                      await reload();
                      setStatus(
                        ['Snapshot deleted.', describeCollection(collected)]
                          .filter(Boolean)
                          .join(' '),
                      );
                    })
                  }
                  className="text-xs text-destructive hover:underline disabled:opacity-50"
                >
                  Delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
