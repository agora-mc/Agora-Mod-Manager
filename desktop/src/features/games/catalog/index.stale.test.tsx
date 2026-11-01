import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { LibraryCatalogSection } from './index';

const entry = (id: string) => ({
  id,
  name: `Entry ${id}`,
  author: null,
  description: null,
  downloadStrategy: 'github_release',
  status: 'ok',
  pageUrl: null,
  compatibility: [],
});

function listing(gameId: string) {
  return {
    entries: [entry(`${gameId}-entry`)],
    instances: [{ instanceId: `${gameId}-inst`, name: `Instance of ${gameId}` }],
  };
}

describe('LibraryCatalogSection: stale selection when the game changes', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockImplementation(async (command: string, args: Record<string, unknown>) => {
      if (command === 'games_catalog_list') return listing(args.gameId as string);
      if (command === 'games_catalog_install') return null;
      return null;
    });
  });

  // Review finding (final-ui). `GameTabPage` is not keyed by game (App.tsx renders it without a
  // `key`), so switching game on the Library tab re-renders this same component with a new
  // `gameId`. `load` keeps the previous game's instance (`current || first`), so the page shows the
  // new game's first instance in the select but sends the OLD game's instance id with Install.
  // Run: npx vitest run src/features/games/catalog/index.stale.test.tsx (un-skip first)
  // proves bug: LibraryCatalogSection keeps the previous game's instanceId after a game switch
  it.skip('installs into the instance the select shows after the game changes', async () => {
    const { rerender } = render(<LibraryCatalogSection gameId="game-a" />);
    await screen.findByText('Entry game-a-entry');
    rerender(<LibraryCatalogSection gameId="game-b" />);
    await screen.findByText('Entry game-b-entry');
    expect(screen.getByRole('combobox')).toHaveDisplayValue('Instance of game-b');

    fireEvent.click(screen.getByRole('button', { name: 'Install' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('games_catalog_install', expect.anything()),
    );
    const call = invokeMock.mock.calls.find(([command]) => command === 'games_catalog_install');
    expect((call?.[1] as { request: { instanceId: string } }).request.instanceId).toBe('game-b-inst');
  });
});
