import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import {
  getIni,
  getModIni,
  getSaves,
  keepModIniGameVersion,
  listModInis,
  resetModIni,
  restoreModIniMine,
  setIniValue,
  setModIniValue,
  setSaves,
  type SavesView,
} from './api';

const folder = { path: 'C:/Saves', exists: true, saves: 2, newest: '2026-10-01T10:00:00Z' };
const savesView: SavesView = {
  instanceId: 'inst-1',
  choice: 'own',
  inUse: folder,
  other: { path: 'C:/Shared', exists: false, saves: 0, newest: null },
  settingFile: 'Skyrim.ini',
  setting: 'General/sSaveFolder',
  settingValue: 'Own',
  settingSource: 'copy',
};

const file = { instancePath: 'Skyrim.ini', copyExists: true, gameFile: 'C:/Game/Skyrim.ini', gameFileExists: true };

describe('INI commands', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('lists the files without a file name, and sends the file when one is named', async () => {
    invokeMock.mockResolvedValueOnce({ files: [file], contents: null });
    await getIni('inst-1');
    expect(invokeMock).toHaveBeenLastCalledWith('games_ini_get', { instanceId: 'inst-1', file: null });

    invokeMock.mockResolvedValueOnce({
      files: [file],
      contents: { instancePath: 'Skyrim.ini', source: 'copy', entries: [{ section: 'General', key: 'a', value: 'b' }] },
    });
    const view = await getIni('inst-1', 'Skyrim.ini');
    expect(invokeMock).toHaveBeenLastCalledWith('games_ini_get', { instanceId: 'inst-1', file: 'Skyrim.ini' });
    expect(view.contents?.entries[0]).toEqual({ section: 'General', key: 'a', value: 'b' });
  });

  it('sends null as the value to remove a key, and a string to set it, even an empty one', async () => {
    invokeMock.mockResolvedValue({ changed: true, view: { files: [file], contents: null } });
    await setIniValue('inst-1', 'Skyrim.ini', 'General', 'sLanguage', null);
    expect(invokeMock).toHaveBeenLastCalledWith('games_ini_set', {
      instanceId: 'inst-1',
      file: 'Skyrim.ini',
      section: 'General',
      key: 'sLanguage',
      value: null,
    });
    await setIniValue('inst-1', 'Skyrim.ini', 'General', 'sLanguage', '');
    expect(invokeMock).toHaveBeenLastCalledWith('games_ini_set', expect.objectContaining({ value: '' }));
  });

  it.each([
    ['null', null],
    ['files that are not a list', { files: 'Skyrim.ini', contents: null }],
    ['a file with no path', { files: [{ copyExists: true }], contents: null }],
    ['an unknown source', { files: [], contents: { instancePath: 'a', source: 'disk', entries: [] } }],
    ['an entry with no value', { files: [], contents: { instancePath: 'a', source: 'copy', entries: [{ section: 'a', key: 'b' }] } }],
  ])('rejects an INI reply that is %s rather than reading it as no keys', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(getIni('inst-1')).rejects.toThrow('games_ini_get did not return the expected INI and saves data.');
  });

  it('rejects a set reply without the changed flag', async () => {
    invokeMock.mockResolvedValueOnce({ view: { files: [], contents: null } });
    await expect(setIniValue('inst-1', 'Skyrim.ini', 'a', 'b', 'c')).rejects.toThrow('games_ini_set did not return');
  });
});

describe('saves commands', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('reads the save choice', async () => {
    invokeMock.mockResolvedValueOnce(savesView);
    await expect(getSaves('inst-1')).resolves.toEqual(savesView);
    expect(invokeMock).toHaveBeenCalledWith('games_saves_get', { instanceId: 'inst-1' });
  });

  it('sends the choice by name, own or shared', async () => {
    invokeMock.mockResolvedValueOnce({
      previous: 'shared',
      setting: { action: 'set', value: 'Own' },
      saves: savesView,
    });
    await setSaves('inst-1', 'own');
    expect(invokeMock).toHaveBeenCalledWith('games_saves_set', { instanceId: 'inst-1', choice: 'own' });
  });

  it.each([
    ['null', null],
    ['a view with no choice', { ...savesView, choice: undefined }],
    ['a choice that is not own or shared', { ...savesView, choice: 'both' }],
    ['a view with no folders', { ...savesView, inUse: undefined }],
    ['a view with a setting source it does not know', { ...savesView, settingSource: 'cloud' }],
  ])('rejects a saves reply that is %s rather than reading it as a default choice', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(getSaves('inst-1')).rejects.toThrow('games_saves_get did not return');
  });

  it('rejects a change with no setting action', async () => {
    invokeMock.mockResolvedValueOnce({ previous: 'own', setting: {}, saves: savesView });
    await expect(setSaves('inst-1', 'shared')).rejects.toThrow('games_saves_set did not return');
  });
});

describe('mod INI commands', () => {
  const modRow = {
    path: 'SKSE/Plugins/SSEDisplayTweaks.ini',
    modId: 'aaaa1111',
    modName: 'SSE Display Tweaks',
    overridden: false,
    shadowedBy: null,
    gameChanged: false,
    modEnabled: true,
  };

  const contents = {
    instancePath: modRow.path,
    source: 'override',
    entries: [{ section: 'Display', key: 'fGamma', value: '2.2' }],
  };

  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('lists the mod INI files for the instance', async () => {
    invokeMock.mockResolvedValueOnce([modRow]);
    await expect(listModInis('inst-1')).resolves.toEqual([modRow]);
    expect(invokeMock).toHaveBeenLastCalledWith('games_mod_inis_list', { instanceId: 'inst-1' });
  });

  it('reads, saves and resets one file by its path', async () => {
    invokeMock.mockResolvedValueOnce(contents);
    await expect(getModIni('inst-1', modRow.path)).resolves.toEqual(contents);
    expect(invokeMock).toHaveBeenLastCalledWith('games_mod_ini_get', {
      instanceId: 'inst-1',
      path: modRow.path,
    });

    invokeMock.mockResolvedValueOnce(contents);
    await setModIniValue('inst-1', modRow.path, 'Display', 'fGamma', '2.2');
    expect(invokeMock).toHaveBeenLastCalledWith('games_mod_ini_set', {
      instanceId: 'inst-1',
      path: modRow.path,
      section: 'Display',
      key: 'fGamma',
      value: '2.2',
    });
    invokeMock.mockResolvedValueOnce(contents);
    await setModIniValue('inst-1', modRow.path, 'Display', 'fGamma', null);
    expect(invokeMock).toHaveBeenLastCalledWith(
      'games_mod_ini_set',
      expect.objectContaining({ value: null }),
    );

    invokeMock.mockResolvedValueOnce([{ ...modRow, overridden: false }]);
    await expect(resetModIni('inst-1', modRow.path)).resolves.toEqual([{ ...modRow, overridden: false }]);
    expect(invokeMock).toHaveBeenLastCalledWith('games_mod_ini_reset', {
      instanceId: 'inst-1',
      path: modRow.path,
    });
  });

  it.each([
    ['null', null],
    ['a row with no mod name', [{ ...modRow, modName: undefined }]],
    ['a row with no mod id', [{ ...modRow, modId: undefined }]],
  ])('rejects a mod INI list that is %s rather than reading it as no files', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(listModInis('inst-1')).rejects.toThrow('games_mod_inis_list did not return');
  });

  it('rejects an entries reply with no value rather than opening an empty editor', async () => {
    invokeMock.mockResolvedValueOnce({
      instancePath: modRow.path,
      source: 'mod',
      entries: [{ section: 'Display', key: 'fGamma' }],
    });
    await expect(getModIni('inst-1', modRow.path)).rejects.toThrow('games_mod_ini_get did not return');
  });

  it('adopts or takes back the game version through their commands', async () => {
    invokeMock.mockResolvedValueOnce([{ ...modRow, gameChanged: true }]);
    await keepModIniGameVersion('inst-1', modRow.path);
    expect(invokeMock).toHaveBeenLastCalledWith('games_mod_ini_keep_game_version', {
      instanceId: 'inst-1',
      path: modRow.path,
    });

    invokeMock.mockResolvedValueOnce([{ ...modRow, gameChanged: false }]);
    await restoreModIniMine('inst-1', modRow.path);
    expect(invokeMock).toHaveBeenLastCalledWith('games_mod_ini_restore_mine', {
      instanceId: 'inst-1',
      path: modRow.path,
    });
  });
});
