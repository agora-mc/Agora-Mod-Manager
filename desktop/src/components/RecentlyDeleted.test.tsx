/**
 * Minecraft's Recently deleted reads the same window as every other game's: the shared
 * `games_recently_deleted_days` setting, in the line under the heading and in the Delete question.
 */
import { render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ConfirmProvider } from '@/components/ui/confirm';
import { getSetting, listRecentlyDeleted } from '../lib/tauri';
import { RecentlyDeleted, deleteDialogBody } from './RecentlyDeleted';

vi.mock('../lib/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/tauri')>();
  return { ...actual, getSetting: vi.fn(), listRecentlyDeleted: vi.fn() };
});

const entry = {
  entry_id: 'e1',
  instance_id: 'pack',
  name: 'Pack',
  minecraft_version: '1.21',
  loader: 'fabric',
  import_source: null,
  deleted_at: '2026-01-01T00:00:00+00:00',
  expires_at: '2026-01-15T00:00:00+00:00',
  size_bytes: 1024 * 1024,
};

beforeEach(() => {
  vi.mocked(getSetting).mockReset();
  vi.mocked(listRecentlyDeleted).mockReset();
});

describe('Minecraft Recently deleted', () => {
  it('says how long entries stay, from the setting every game shares', async () => {
    vi.mocked(getSetting).mockImplementation(async (key: string) => (key === 'games_recently_deleted_days' ? 14 : null) as never);
    vi.mocked(listRecentlyDeleted).mockResolvedValue([entry] as never);
    render(
      <ConfirmProvider>
        <RecentlyDeleted refreshKey={0} onRestored={() => undefined} />
      </ConfirmProvider>,
    );
    const region = await screen.findByRole('region', { name: 'Recently deleted' });
    await waitFor(() => expect(region).toHaveTextContent('stay here for 14 days'));
    expect(getSetting).toHaveBeenCalledWith('games_recently_deleted_days');
  });

  it('keeps 30 days when the setting is missing', async () => {
    vi.mocked(getSetting).mockResolvedValue(null as never);
    vi.mocked(listRecentlyDeleted).mockResolvedValue([entry] as never);
    render(
      <ConfirmProvider>
        <RecentlyDeleted refreshKey={0} onRestored={() => undefined} />
      </ConfirmProvider>,
    );
    const region = await screen.findByRole('region', { name: 'Recently deleted' });
    expect(region).toHaveTextContent('stay here for 30 days');
  });

  it('puts the window in the Delete question', () => {
    expect(deleteDialogBody({ name: 'Pack' }, 1)).toContain('Recently deleted for 1 day.');
    expect(deleteDialogBody({ name: 'Pack' }, 60)).toContain('Recently deleted for 60 days.');
    expect(deleteDialogBody({ name: 'Pack' })).toContain('Recently deleted for 30 days.');
  });
});
