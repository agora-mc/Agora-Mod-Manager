import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { GAME_SUPPORT_CHANGED_EVENT, listGameSupport, setGameEnabled, type GameSupportRow } from './api';

const minecraft: GameSupportRow = {
  gameId: 'minecraft',
  name: 'Minecraft: Java Edition',
  source: { kind: 'builtIn' },
  enabled: true,
  canDisable: false,
};
const valheim: GameSupportRow = {
  gameId: 'valheim',
  name: 'Valheim',
  source: { kind: 'plugin', pluginId: 'agora-tracer.valheim' },
  enabled: false,
  canDisable: true,
};

describe('listGameSupport', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('returns the rows the backend lists, plugin source included', async () => {
    invokeMock.mockResolvedValueOnce([minecraft, valheim]);
    await expect(listGameSupport()).resolves.toEqual([minecraft, valheim]);
    expect(invokeMock).toHaveBeenCalledWith('games_support_list');
  });

  it('accepts an empty list (no games at all is a real answer)', async () => {
    invokeMock.mockResolvedValueOnce([]);
    await expect(listGameSupport()).resolves.toEqual([]);
  });

  it.each([
    ['null (an unmocked command answers this)', null],
    ['an object instead of a list', { games: [minecraft] }],
    ['a list with a null row', [null]],
    ['a row without a game id', [{ ...minecraft, gameId: undefined }]],
    ['an empty game id', [{ ...minecraft, gameId: '' }]],
    ['a row without a name', [{ ...minecraft, name: 42 }]],
    ['a row without a source', [{ ...minecraft, source: undefined }]],
    ['an unknown source kind', [{ ...minecraft, source: { kind: 'steam' } }]],
    ['a plugin source without its id', [{ ...valheim, source: { kind: 'plugin' } }]],
    ['a plugin source with an empty id', [{ ...valheim, source: { kind: 'plugin', pluginId: '' } }]],
    ['enabled as a string', [{ ...valheim, enabled: 'false' }]],
    ['canDisable missing', [{ ...valheim, canDisable: undefined }]],
    ['a locked game shown as off', [{ ...minecraft, enabled: false }]],
    ['the same game twice', [valheim, { ...valheim }]],
  ])('rejects %s rather than reading it as a clean list', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(listGameSupport()).rejects.toThrow(/games_support_list did not return a list of games|returned the same game twice/);
  });

  it('rejects when the command itself fails', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'core_failed', message: 'damaged', coreCode: 'ERR_GAME_SUPPORT_MALFORMED' });
    await expect(listGameSupport()).rejects.toMatchObject({ coreCode: 'ERR_GAME_SUPPORT_MALFORMED' });
  });
});

describe('setGameEnabled', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('sends the id and the state, and announces the change on success', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    const heard = vi.fn();
    window.addEventListener(GAME_SUPPORT_CHANGED_EVENT, heard);
    try {
      await setGameEnabled('valheim', true);
      expect(invokeMock).toHaveBeenCalledWith('games_support_set_enabled', { gameId: 'valheim', enabled: true });
      expect(heard).toHaveBeenCalledTimes(1);
    } finally {
      window.removeEventListener(GAME_SUPPORT_CHANGED_EVENT, heard);
    }
  });

  it('rejects with the core reason and announces nothing when the game is refused', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'core_failed', message: 'Minecraft cannot be turned off yet.', coreCode: 'ERR_GAME_SUPPORT_CANNOT_DISABLE' });
    const heard = vi.fn();
    window.addEventListener(GAME_SUPPORT_CHANGED_EVENT, heard);
    try {
      await expect(setGameEnabled('minecraft', false)).rejects.toMatchObject({ coreCode: 'ERR_GAME_SUPPORT_CANNOT_DISABLE' });
      expect(heard).not.toHaveBeenCalled();
    } finally {
      window.removeEventListener(GAME_SUPPORT_CHANGED_EVENT, heard);
    }
  });
});
