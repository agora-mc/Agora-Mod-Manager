import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import {
  addLibraryItem,
  filterLibraryByGame,
  instancesForItem,
  listLibrary,
  listLibraryInstances,
  loadLibraryIcon,
  searchLibrary,
  type LibraryItem,
} from './api';

const first: LibraryItem = {
  itemId: 'a'.repeat(64),
  name: 'First item',
  description: 'A saved description.',
  version: '1.0.0',
  gameId: 'minecraft',
  pageUrl: null,
  iconSha256: null,
  sourceKind: 'file',
  sourceLabel: 'File: first.jar',
  instances: [],
};

const second: LibraryItem = {
  ...first,
  itemId: 'b'.repeat(64),
  name: 'Second item',
  description: null,
  version: '2.0.0',
  gameId: 'skyrim-se',
  sourceLabel: 'Archive: second.zip',
};

const installedMod = {
  filename: 'first.jar',
  registry_id: null,
  modrinth_id: null,
  source: 'manual',
  version: '1.0.0',
  sha256: 'd'.repeat(64),
  installed_at: '2026-01-01T00:00:00Z',
  enabled: true,
  content_type: 'mod',
  content_item_id: first.itemId,
  object_sha256: 'd'.repeat(64),
};

const minecraftInstance = { instanceId: 'mc1', name: 'Minecraft One', gameId: 'minecraft' };
const skyrimInstance = { instanceId: 'sky1', name: 'Skyrim One', gameId: 'skyrim-se' };

describe('listLibrary', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('returns the items and damaged records the backend lists', async () => {
    const listing = { items: [first], damaged: [], warnings: [] };
    invokeMock.mockResolvedValueOnce(listing);
    await expect(listLibrary()).resolves.toEqual(listing);
    expect(invokeMock).toHaveBeenCalledWith('games_library_list');
  });

  it('rejects a reply that is not a library listing rather than showing an empty one', async () => {
    invokeMock.mockResolvedValueOnce(null);
    await expect(listLibrary()).rejects.toThrow('games_library_list did not return a library listing.');
  });

  it('passes a backend error through', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'core_failed', message: 'The store index is damaged.' });
    await expect(listLibrary()).rejects.toMatchObject({ code: 'core_failed' });
  });
});

describe('listLibraryInstances', () => {
  it('returns every game instance', async () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValueOnce({ instances: [minecraftInstance, skyrimInstance], warnings: [] });
    await expect(listLibraryInstances()).resolves.toEqual({
      instances: [minecraftInstance, skyrimInstance],
      warnings: [],
    });
  });
});

describe('addLibraryItem', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('adds a Minecraft item with the Minecraft store command', async () => {
    invokeMock.mockResolvedValueOnce(installedMod);
    await expect(addLibraryItem(first, minecraftInstance)).resolves.toEqual({ status: 'added' });
    expect(invokeMock).toHaveBeenCalledWith('minecraft_add_stored_content', {
      instanceId: 'mc1',
      itemId: first.itemId,
    });
  });

  it('passes the Minecraft command failure through', async () => {
    invokeMock.mockRejectedValueOnce({
      code: 'ERR_CONTENT_ITEM_MISSING',
      message: 'The store item is missing.',
    });
    await expect(addLibraryItem(first, minecraftInstance)).rejects.toMatchObject({
      code: 'ERR_CONTENT_ITEM_MISSING',
    });
  });

  it('rejects a Minecraft reply that is not an installed mod', async () => {
    invokeMock.mockResolvedValueOnce({});
    await expect(addLibraryItem(first, minecraftInstance)).rejects.toThrow(
      'minecraft_add_stored_content did not return the installed mod.',
    );
  });

  it('adds another game’s item with the generic content command', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'added' });
    await expect(addLibraryItem(second, skyrimInstance)).resolves.toEqual({ status: 'added' });
    expect(invokeMock).toHaveBeenCalledWith('games_instance_content_add', {
      instanceId: 'sky1',
      itemId: second.itemId,
    });
  });

  it('reports an installer that needs answers instead of claiming success', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'needs_installer' });
    await expect(addLibraryItem(second, skyrimInstance)).resolves.toEqual({
      status: 'needs_installer',
    });
  });
});

describe('loadLibraryIcon', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('reads one icon by hash and keeps it for the session', async () => {
    const hash = 'd'.repeat(64);
    const dataUrl = 'data:image/png;base64,AAAA';
    invokeMock.mockResolvedValue(dataUrl);
    await expect(loadLibraryIcon(hash)).resolves.toBe(dataUrl);
    await expect(loadLibraryIcon(hash)).resolves.toBe(dataUrl);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith('games_library_icon', { sha256: hash });
  });

  it('treats a reply that is not an image as no icon', async () => {
    invokeMock.mockResolvedValue('not an image');
    await expect(loadLibraryIcon('e'.repeat(64))).resolves.toBeNull();
  });

  it('treats a failed read as no icon', async () => {
    invokeMock.mockRejectedValue({ code: 'core_failed', message: 'damaged' });
    await expect(loadLibraryIcon('f'.repeat(64))).resolves.toBeNull();
  });
});

describe('search and filters', () => {
  it('filters by game and keeps items with no recorded game out of a game filter', () => {
    const unrecorded = { ...first, itemId: 'c'.repeat(64), gameId: null };
    expect(filterLibraryByGame([first, second, unrecorded], 'minecraft')).toEqual([first]);
    expect(filterLibraryByGame([first, second], null)).toHaveLength(2);
  });

  it('searches names, descriptions, versions and sources', () => {
    expect(searchLibrary([first, second], 'saved')).toEqual([first]);
    expect(searchLibrary([first, second], 'second.zip')).toEqual([second]);
    expect(searchLibrary([first, second], '2.0.0')).toEqual([second]);
    expect(searchLibrary([first, second], '')).toHaveLength(2);
  });

  it('offers only instances of the item’s game', () => {
    expect(instancesForItem(first, [minecraftInstance, skyrimInstance])).toEqual([
      minecraftInstance,
    ]);
    expect(instancesForItem({ ...first, gameId: null }, [minecraftInstance, skyrimInstance]))
      .toHaveLength(2);
  });
});
