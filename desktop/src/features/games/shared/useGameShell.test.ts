import { act, renderHook, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { GAME_SUPPORT_CHANGED_EVENT, type GameSupportRow } from '../support/api';
import { useGameShell, visibleGames } from './useGameShell';
import type { GameListing } from './types';

const skyrim: GameListing = {
  gameId: 'skyrim-se',
  name: 'Skyrim Special Edition',
  installs: [],
};
const valheim: GameListing = {
  gameId: 'valheim',
  name: 'Valheim',
  installs: [],
};
const minecraftRow: GameSupportRow = {
  gameId: 'minecraft',
  name: 'Minecraft: Java Edition',
  source: { kind: 'builtIn' },
  enabled: true,
  canDisable: false,
};
const skyrimRow = (enabled: boolean): GameSupportRow => ({
  gameId: 'skyrim-se',
  name: 'Skyrim Special Edition',
  source: { kind: 'builtIn' },
  enabled,
  canDisable: true,
});

/** Answers the commands the shell reads. A value that is an Error is thrown instead. */
function answer(replies: Record<string, unknown>) {
  invokeMock.mockImplementation(async (command: string) => {
    const reply = replies[command];
    if (reply instanceof Error) throw reply;
    return reply === undefined ? null : reply;
  });
}

const baseReplies = {
  get_setting: 'skyrim-se',
  games_list: { games: [{ gameId: 'skyrim-se', name: 'Skyrim Special Edition', installs: [{ installId: 'steam:489830' }] }] },
};

describe('visibleGames', () => {
  it('drops the games the user has turned off', () => {
    expect(visibleGames([skyrim, valheim], [minecraftRow, skyrimRow(false)]).map((g) => g.gameId)).toEqual(['valheim']);
  });

  it('hides nothing when the support list did not load', () => {
    expect(visibleGames([skyrim, valheim], null)).toEqual([skyrim, valheim]);
  });

  it('ignores a support row for a game that is not found, and keeps games with no row', () => {
    expect(visibleGames([valheim], [skyrimRow(false)])).toEqual([valheim]);
  });

  it('returns nothing for nothing found', () => {
    expect(visibleGames([], [skyrimRow(false)])).toEqual([]);
  });
});

describe('useGameShell with the Game support list', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('falls back to Minecraft and offers no game when the active game is turned off', async () => {
    answer({ ...baseReplies, games_support_list: [minecraftRow, skyrimRow(false)] });
    const { result } = renderHook(() => useGameShell());
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.shownGame).toBe('minecraft');
    expect(result.current.foundGames).toEqual([]);
    expect(result.current.switcherVisible).toBe(false);
    // The stored choice is kept, so turning the game back on brings it back.
    expect(result.current.activeGame).toBe('skyrim-se');
  });

  it('shows the active game again once it is turned back on', async () => {
    answer({ ...baseReplies, games_support_list: [minecraftRow, skyrimRow(true)] });
    const { result } = renderHook(() => useGameShell());
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.shownGame).toBe('skyrim-se');
    expect(result.current.foundGames.map((g) => g.gameId)).toEqual(['skyrim-se']);
  });

  it('hides nothing when the support list fails to load', async () => {
    answer({ ...baseReplies, games_support_list: new Error('games_support_list did not return a list of games.') });
    const { result } = renderHook(() => useGameShell());
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.foundGames.map((g) => g.gameId)).toEqual(['skyrim-se']);
    expect(result.current.shownGame).toBe('skyrim-se');
  });

  it('hides nothing when the support list reply is not a list', async () => {
    answer({ ...baseReplies, games_support_list: { games: [] } });
    const { result } = renderHook(() => useGameShell());
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.foundGames.map((g) => g.gameId)).toEqual(['skyrim-se']);
  });

  it('is not ready until the support list has answered', async () => {
    let releaseSupport: (value: unknown) => void = () => undefined;
    invokeMock.mockImplementation((command: string) => {
      if (command === 'games_support_list') {
        return new Promise((resolve) => {
          releaseSupport = resolve;
        });
      }
      if (command === 'get_setting') return Promise.resolve('skyrim-se');
      return Promise.resolve(baseReplies.games_list);
    });
    const { result } = renderHook(() => useGameShell());
    // Discovery and the stored choice are in; the support list is not, so the shell must wait.
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('games_support_list'));
    expect(result.current.ready).toBe(false);
    await act(async () => {
      releaseSupport([minecraftRow, skyrimRow(false)]);
    });
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.shownGame).toBe('minecraft');
  });

  it('keeps the found list the same array when only the chosen game changes', async () => {
    answer({ ...baseReplies, games_support_list: [minecraftRow, skyrimRow(true)] });
    const { result } = renderHook(() => useGameShell());
    await waitFor(() => expect(result.current.ready).toBe(true));
    const before = result.current.foundGames;
    act(() => result.current.setActiveGame('minecraft'));
    await waitFor(() => expect(result.current.activeGame).toBe('minecraft'));
    // A new list on every render makes every consumer re-render, so a filtered
    // view must keep the same identity while discovery and support are unchanged.
    expect(result.current.foundGames).toBe(before);
  });

  it('follows a toggle without a restart: the change event rereads the list', async () => {
    let supportRows: unknown = [minecraftRow, skyrimRow(false)];
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'games_support_list') return supportRows;
      if (command === 'get_setting') return 'skyrim-se';
      return baseReplies.games_list;
    });
    const { result } = renderHook(() => useGameShell());
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.foundGames).toEqual([]);

    supportRows = [minecraftRow, skyrimRow(true)];
    await act(async () => {
      window.dispatchEvent(new Event(GAME_SUPPORT_CHANGED_EVENT));
    });
    await waitFor(() => expect(result.current.foundGames.map((g) => g.gameId)).toEqual(['skyrim-se']));
    expect(result.current.shownGame).toBe('skyrim-se');
  });

  it('a failed reread after a toggle hides nothing rather than keeping a stale hide', async () => {
    let fail = false;
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'games_support_list') {
        if (fail) throw new Error('games_support_list failed');
        return [minecraftRow, skyrimRow(false)];
      }
      if (command === 'get_setting') return 'skyrim-se';
      return baseReplies.games_list;
    });
    const { result } = renderHook(() => useGameShell());
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.foundGames).toEqual([]);

    fail = true;
    await act(async () => {
      window.dispatchEvent(new Event(GAME_SUPPORT_CHANGED_EVENT));
    });
    await waitFor(() => expect(result.current.foundGames.map((g) => g.gameId)).toEqual(['skyrim-se']));
  });
});
