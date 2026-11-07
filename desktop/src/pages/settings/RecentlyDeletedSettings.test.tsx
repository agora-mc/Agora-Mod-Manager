/**
 * The Recently deleted window in General > Storage, shared by every game: read the way core reads it (a wild value
 * is clamped, a missing one is 30), offered with 30 marked recommended, and saved as a number.
 */
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { getSetting, setSetting } from '../../lib/tauri';
import { parseRetentionDays } from '../../lib/recentlyDeletedDays';
import { RecentlyDeletedSettings } from './RecentlyDeletedSettings';

vi.mock('../../lib/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../lib/tauri')>();
  return { ...actual, getSetting: vi.fn(), setSetting: vi.fn() };
});

beforeEach(() => {
  vi.mocked(getSetting).mockReset();
  vi.mocked(setSetting).mockReset();
});

describe('parseRetentionDays', () => {
  it('reads numbers and numeric text, clamps wild values, and defaults to 30', () => {
    expect(parseRetentionDays(14)).toBe(14);
    expect(parseRetentionDays(' 60 ')).toBe(60);
    expect(parseRetentionDays(0)).toBe(1);
    expect(parseRetentionDays(5000)).toBe(366);
    expect(parseRetentionDays(null)).toBe(30);
    expect(parseRetentionDays('soon')).toBe(30);
  });
});

describe('RecentlyDeletedSettings', () => {
  it('shows the saved window, recommends 30 days, and saves a new one as a number', async () => {
    vi.mocked(getSetting).mockResolvedValue(45);
    vi.mocked(setSetting).mockResolvedValue(undefined as never);
    render(<RecentlyDeletedSettings />);
    const select = await screen.findByRole('combobox', { name: 'Keep deleted instances for' });
    expect(select).toHaveValue('45');
    expect(screen.getByRole('option', { name: '30 days (recommended)' })).toBeInTheDocument();
    fireEvent.change(select, { target: { value: '7' } });
    await waitFor(() => expect(setSetting).toHaveBeenCalledWith('games_recently_deleted_days', 7));
  });
});
