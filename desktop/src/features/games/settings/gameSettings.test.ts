import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import {
  DEFAULT_GAME_SETTINGS,
  gameSettingKey,
  loadGameSettings,
  loadGameSettingsOrDefaults,
  saveGameSetting,
} from './gameSettings';

/** Answer get_setting from a key/value map; any other command is unexpected. */
function store(values: Record<string, unknown>) {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === 'get_setting') {
      const key = String((args as { key?: unknown } | undefined)?.key ?? '');
      return Object.prototype.hasOwnProperty.call(values, key) ? values[key] : null;
    }
    throw new Error(`unexpected command ${command}`);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe('per-game settings', () => {
  it('uses the safe defaults for a game with nothing saved', async () => {
    store({});
    await expect(loadGameSettings('skyrim-se')).resolves.toEqual({
      baseType: 'linked',
      deployment: 'auto',
      importCopySaves: 'off',
    });
    expect(invokeMock.mock.calls.map(([command, args]) => [command, args])).toEqual([
      ['get_setting', { key: 'game.skyrim-se.baseType' }],
      ['get_setting', { key: 'game.skyrim-se.deployment' }],
      ['get_setting', { key: 'game.skyrim-se.importCopySaves' }],
    ]);
  });

  it('keeps one game\'s values apart from another game\'s', async () => {
    store({
      [gameSettingKey('skyrim-se', 'baseType')]: 'copied',
      [gameSettingKey('skyrim-se', 'deployment')]: 'copies',
      [gameSettingKey('skyrim-se', 'importCopySaves')]: 'on',
    });

    await expect(loadGameSettings('skyrim-se')).resolves.toEqual({
      baseType: 'copied',
      deployment: 'copies',
      importCopySaves: 'on',
    });
    await expect(loadGameSettings('valheim')).resolves.toEqual({
      baseType: 'linked',
      deployment: 'auto',
      importCopySaves: 'off',
    });
  });

  it('refuses a stored value Agora does not understand instead of reading it as the default', async () => {
    store({ [gameSettingKey('skyrim-se', 'deployment')]: 'teleport' });
    await expect(loadGameSettings('skyrim-se')).rejects.toThrow(
      'game.skyrim-se.deployment is set to "teleport"',
    );
  });

  it('falls back to the defaults only where a preset is all the caller needs', async () => {
    store({ [gameSettingKey('skyrim-se', 'baseType')]: 'teleport' });
    await expect(loadGameSettingsOrDefaults('skyrim-se')).resolves.toEqual(DEFAULT_GAME_SETTINGS);
  });

  it('writes under a key that carries the game id', async () => {
    store({});
    invokeMock.mockResolvedValueOnce(undefined);
    await saveGameSetting('skyrim-se', 'deployment', 'links');
    expect(invokeMock).toHaveBeenCalledWith('set_setting', {
      key: 'game.skyrim-se.deployment',
      value: 'links',
    });
  });
});
