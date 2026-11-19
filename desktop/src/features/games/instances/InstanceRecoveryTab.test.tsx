/**
 * A game instance shows its last-known-good setup, the changes since it, and
 * restores through the same guarded snapshot command as Minecraft.
 */
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider } from '@/components/ui/confirm';
import { InstanceRecoveryTab } from './InstanceRecoveryTab';
import {
  detectDrift,
  importBackup,
  listSnapshots,
  pickOpenFile,
  previewBackupImport,
  restoreSnapshot,
} from '@/lib/tauri';

const lkg = {
  id: 'snap-good',
  label: 'pre-launch',
  created_at: '2026-11-01T10:00:00Z',
  file_count: 4,
  size_estimate: 1024,
  effective_origin: 'automatic' as const,
  is_lkg: true,
  is_current_lkg: true,
  is_pre_restore: false,
};

vi.mock('@/lib/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/lib/tauri')>();
  return {
    ...actual,
    listSnapshots: vi.fn(),
    createSnapshot: vi.fn(),
    deleteSnapshot: vi.fn(),
    detectDrift: vi.fn(),
    exportBackup: vi.fn(),
    importBackup: vi.fn(),
    previewBackupImport: vi.fn(),
    gameBackupBytes: vi.fn(async () => 2048),
    pickDirectory: vi.fn(async () => '/tmp/backups'),
    pickOpenFile: vi.fn(async () => null),
    restoreSnapshot: vi.fn(async () => ({})),
  };
});

describe('InstanceRecoveryTab', () => {
  beforeEach(() => {
    vi.mocked(listSnapshots).mockResolvedValue([lkg]);
    vi.mocked(detectDrift).mockResolvedValue({
      fromId: lkg.id,
      toId: null,
      added: [],
      removed: [],
      modified: [
        {
          path: 'user/Plugins.txt',
          oldSha256: 'a',
          newSha256: 'b',
          oldSize: 10,
          newSize: 10,
        },
      ],
      unchangedCount: 3,
      totalFilesA: 4,
      totalFilesB: 4,
    });
    vi.mocked(restoreSnapshot).mockClear();
  });

  it('shows the last-good setup and the changes since it', async () => {
    render(<InstanceRecoveryTab instanceId="inst" />);

    expect((await screen.findAllByText(/Last setup that launched fine/)).length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole('button', { name: 'Changes since then' }));

    await waitFor(() => expect(detectDrift).toHaveBeenCalledWith('inst', 'snap-good'));
    expect(await screen.findByText('~ user/Plugins.txt')).toBeVisible();
  });

  it('restores to the last-good setup through the guarded command', async () => {
    render(<InstanceRecoveryTab instanceId="inst" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Restore to it' }));

    await waitFor(() => expect(restoreSnapshot).toHaveBeenCalledWith('inst', 'snap-good', false));
  });

  it('asks before fetching missing content and imports with the fetch flag', async () => {
    vi.mocked(pickOpenFile).mockResolvedValueOnce('/tmp/backup.zip');
    vi.mocked(previewBackupImport).mockResolvedValueOnce({
      snapshotId: 'backup-snap',
      missingItems: ['Fake mod'],
      missingObjects: 1,
      missingBytes: 4096,
      totalItems: 1,
    });
    vi.mocked(importBackup).mockResolvedValueOnce({} as never);

    const { container } = render(
      <ConfirmProvider>
        <InstanceRecoveryTab instanceId="inst" />
      </ConfirmProvider>,
    );
    fireEvent.click(await within(container).findByRole('button', { name: /Import backup/ }));
    fireEvent.click(await screen.findByRole('button', { name: 'Fetch and import' }));

    await waitFor(() =>
      expect(importBackup).toHaveBeenCalledWith('inst', '/tmp/backup.zip', true),
    );
  });

  it('shows the size before an export that would include the mod files', async () => {
    const { gameBackupBytes } = await import('@/lib/tauri');
    render(<InstanceRecoveryTab instanceId="inst" />);

    fireEvent.click(await screen.findByRole('button', { name: 'Export…' }));
    await waitFor(() => expect(gameBackupBytes).toHaveBeenCalledWith('inst', 'snap-good'));
  });
});
