import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
const listenMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: listenMock }));

import {
  checkGameInstance,
  cloneGameInstance,
  createGameInstance,
  gamesErrorCode,
  gamesErrorMessage,
  getGameInstance,
  launchGameInstance,
  listDeletedGameInstances,
  listGameInstances,
  onCreateProgress,
  onInstanceExited,
  purgeDeletedGameInstances,
  renameGameInstance,
  restoreDeletedGameInstance,
  setGameInstanceDeployment,
  stopGameInstance,
} from './api';

const capabilities = {
  launch: true,
  stop: true,
  rename: true,
  clone: true,
  trash: true,
  openFolder: true,
  playTime: true,
};

const card = {
  instanceId: 'grounded-1',
  gameId: 'skyrim-se',
  name: 'Grounded',
  store: 'steam',
  version: '1.6.1170',
  pinned: true,
  deployment: null,
  lastLaunchedAt: null,
  lastPlayedAt: null,
  playTimeMs: 0,
  running: false,
  capabilities,
};

const framework = {
  kind: 'framework',
  blocks: true,
  title: 'Script extender',
  detail: 'It needs skse64.dll, but the game has no file of its family.',
  why: 'A framework built for another game version often stops the game from starting.',
  repair: 'Install the matching extender.',
};

describe('listGameInstances', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('passes the game id and returns the cards', async () => {
    invokeMock.mockResolvedValueOnce({ instances: [card] });
    await expect(listGameInstances('skyrim-se')).resolves.toEqual({ instances: [card] });
    expect(invokeMock).toHaveBeenCalledWith('games_instance_list', { gameId: 'skyrim-se' });
  });

  it('returns an empty list only when the reply says so', async () => {
    invokeMock.mockResolvedValueOnce({ instances: [] });
    await expect(listGameInstances('skyrim-se')).resolves.toEqual({ instances: [] });
  });

  it.each([
    ['null (an unmocked command answers this)', null],
    ['a bare array', [card]],
    ['an object without instances', { cards: [card] }],
    ['a card without a name', { instances: [{ ...card, name: undefined }] }],
    ['a card whose deployment is not a mode', { instances: [{ ...card, deployment: 'everything' }] }],
    ['a card whose pinned flag is text', { instances: [{ ...card, pinned: 'yes' }] }],
    ['a card with no capabilities', { instances: [{ ...card, capabilities: undefined }] }],
    ['a card whose play time is text', { instances: [{ ...card, playTimeMs: '7h' }] }],
    ['a card with a capability that is not a boolean', { instances: [{ ...card, capabilities: { ...capabilities, stop: 'yes' } }] }],
  ])('rejects %s rather than reading it as no instances', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(listGameInstances('skyrim-se')).rejects.toThrow('games_instance_list did not return what the Instances page expects.');
  });

  it('passes a rejected command through as the rejection', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'invalid_request', message: 'Minecraft instances are listed under My Instances, not here.' });
    await expect(listGameInstances('minecraft')).rejects.toMatchObject({ code: 'invalid_request' });
  });
});

describe('getGameInstance', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('rejects a page reply that has no saves choice', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', name: 'Grounded', pinned: true, running: false, saves: 'everyone' });
    await expect(getGameInstance('grounded-1')).rejects.toThrow('games_instance_get did not return');
  });

  it('accepts a complete page reply', async () => {
    const detail = {
      instanceId: 'grounded-1',
      gameId: 'skyrim-se',
      gameName: 'Skyrim Special Edition',
      name: 'Grounded',
      pinned: true,
      store: 'steam',
      version: '1.6.1170',
      build: null,
      baseId: 'base-1',
      installId: null,
      unpinnedReason: null,
      deployment: 'links',
      saves: 'shared',
      createdAt: '2026-10-01 10:00:00',
      lastLaunchedAt: null,
      lastPlayedAt: '2026-10-02 09:00:00',
      playTimeMs: 7_200_000,
      running: false,
      capabilities,
    };
    invokeMock.mockResolvedValueOnce(detail);
    await expect(getGameInstance('grounded-1')).resolves.toEqual(detail);
    expect(invokeMock).toHaveBeenCalledWith('games_instance_get', { instanceId: 'grounded-1' });
  });
});

describe('checkGameInstance', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('accepts findings with their kind and whether they block', async () => {
    const check = { instanceId: 'grounded-1', checked: true, status: 'findings', findings: [framework], passed: [] };
    invokeMock.mockResolvedValueOnce(check);
    await expect(checkGameInstance('grounded-1')).resolves.toEqual(check);
  });

  it('accepts the checks that ran and passed', async () => {
    const passed = { kind: 'framework', title: 'Skyrim Script Extender (SKSE)', detail: 'Found skse64_1_6_1170.dll, which matches this game version.' };
    const check = { instanceId: 'grounded-1', checked: true, status: 'ok', findings: [], passed: [passed] };
    invokeMock.mockResolvedValueOnce(check);
    await expect(checkGameInstance('grounded-1')).resolves.toEqual(check);
  });

  it.each([
    ['an unknown finding kind', { ...framework, kind: 'mystery' }],
    ['a finding without a repair field', { ...framework, repair: undefined, detail: 7 }],
    ['a finding that is null', null],
  ])('rejects %s', async (_label, finding) => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', checked: true, status: 'findings', findings: [finding], passed: [] });
    await expect(checkGameInstance('grounded-1')).rejects.toThrow('games_instance_check did not return');
  });

  it.each([
    ['a reply with no passed list', { instanceId: 'grounded-1', checked: true, status: 'ok', findings: [] }],
    ['a passed list that is not a list', { instanceId: 'grounded-1', checked: true, status: 'ok', findings: [], passed: 'all good' }],
    ['a passed check with an unknown kind', { instanceId: 'grounded-1', checked: true, status: 'ok', findings: [], passed: [{ kind: 'mystery', title: 'x', detail: 'y' }] }],
    ['a passed check with no detail', { instanceId: 'grounded-1', checked: true, status: 'ok', findings: [], passed: [{ kind: 'framework', title: 'x' }] }],
  ])('rejects %s', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(checkGameInstance('grounded-1')).rejects.toThrow('games_instance_check did not return');
  });

  it('rejects a check whose status is not one the page knows', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', checked: true, status: 'fine', findings: [], passed: [] });
    await expect(checkGameInstance('grounded-1')).rejects.toThrow('games_instance_check did not return');
  });
});

describe('launchGameInstance', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('sends the launch-anyway flag and the one-off deployment, and null when there is none', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'launched', pid: 4242, deployment: 'links', notice: null, findings: [], reason: null, next: null });
    await launchGameInstance('grounded-1', { launchAnyway: false });
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_launch', {
      instanceId: 'grounded-1',
      launchAnyway: false,
      deployment: null,
    });

    invokeMock.mockResolvedValueOnce({ status: 'launched', pid: 4243, deployment: 'copies', notice: null, findings: [], reason: null, next: null });
    await launchGameInstance('grounded-1', { launchAnyway: true, deployment: 'copies' });
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_launch', {
      instanceId: 'grounded-1',
      launchAnyway: true,
      deployment: 'copies',
    });
  });

  it('returns a refused launch with its findings, and a fallback question with its next rung', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'refused', pid: null, deployment: null, notice: null, findings: [framework], reason: null, next: null });
    await expect(launchGameInstance('grounded-1', { launchAnyway: false })).resolves.toMatchObject({ status: 'refused' });

    invokeMock.mockResolvedValueOnce({ status: 'vfs_unavailable', pid: null, deployment: null, notice: null, findings: [], reason: 'no driver', next: 'links' });
    await expect(launchGameInstance('grounded-1', { launchAnyway: false })).resolves.toMatchObject({ status: 'vfs_unavailable', next: 'links' });
  });

  it.each([
    ['a status nobody defined', { status: 'maybe', pid: null, deployment: null, notice: null, findings: [], reason: null, next: null }],
    ['a pid that is text', { status: 'launched', pid: '4242', deployment: null, notice: null, findings: [], reason: null, next: null }],
    ['a fallback rung that is not a mode', { status: 'vfs_unavailable', pid: null, deployment: null, notice: null, findings: [], reason: 'x', next: 'direct' }],
    ['no reply at all', undefined],
  ])('rejects %s rather than reading it as a launch', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(launchGameInstance('grounded-1', { launchAnyway: false })).rejects.toThrow('games_instance_launch did not return');
  });
});

describe('create, delete and deployment wrappers', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('sends the install, name and base mode the backend reads', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', name: 'Grounded', base: 'pinned', reusedBase: true, baseId: 'b', unpinnedReason: null });
    await createGameInstance('skyrim-se', 'steam-489830', 'Grounded', 'linked', 'links');
    expect(invokeMock).toHaveBeenCalledWith('games_instance_create', {
      gameId: 'skyrim-se',
      installId: 'steam-489830',
      name: 'Grounded',
      baseMode: 'linked',
      deployment: 'links',
    });
  });

  it('rejects a deployment change that answers with a mode the page does not know', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', deployment: 'direct' });
    await expect(setGameInstanceDeployment('grounded-1', 'links')).rejects.toThrow('games_instance_set_deployment did not return');
  });

  it('accepts an automatic deployment as null', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', deployment: null });
    await expect(setGameInstanceDeployment('grounded-1', 'auto')).resolves.toEqual({ instanceId: 'grounded-1', deployment: null });
    expect(invokeMock).toHaveBeenCalledWith('games_instance_set_deployment', { instanceId: 'grounded-1', mode: 'auto' });
  });
});

describe('rename, clone, stop and Recently deleted wrappers', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('renames and clones with the fields the backend reads', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', name: 'Grounded Again' });
    await expect(renameGameInstance('grounded-1', 'Grounded Again')).resolves.toEqual({
      instanceId: 'grounded-1',
      name: 'Grounded Again',
    });
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_rename', {
      instanceId: 'grounded-1',
      name: 'Grounded Again',
    });

    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-2', name: 'Grounded Copy' });
    await expect(cloneGameInstance('grounded-1', 'Grounded Copy')).resolves.toEqual({
      instanceId: 'grounded-2',
      name: 'Grounded Copy',
    });
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_clone', {
      instanceId: 'grounded-1',
      name: 'Grounded Copy',
      copySaves: false,
    });

    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-2', name: 'Grounded Copy' });
    await cloneGameInstance('grounded-1', 'Grounded Copy', true);
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_clone', {
      instanceId: 'grounded-1',
      name: 'Grounded Copy',
      copySaves: true,
    });
  });

  it('stops an instance and reads the play time it reports', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', processesEnded: 2, playTimeMs: 7_200_000 });
    await expect(stopGameInstance('grounded-1')).resolves.toEqual({
      instanceId: 'grounded-1',
      processesEnded: 2,
      playTimeMs: 7_200_000,
    });
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_stop', { instanceId: 'grounded-1' });
  });

  it('lists Recently deleted for one game with the retention window', async () => {
    const entry = {
      entryId: 'entry-1',
      instanceId: 'grounded-1',
      gameId: 'skyrim-se',
      name: 'Grounded',
      deletedAt: '2026-10-01T10:00:00Z',
      expiresAt: '2026-10-31T10:00:00Z',
      sizeBytes: 1234,
    };
    invokeMock.mockResolvedValueOnce({ entries: [entry], retentionDays: 30 });
    await expect(listDeletedGameInstances('skyrim-se')).resolves.toEqual({ entries: [entry], retentionDays: 30 });
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_deleted_list', { gameId: 'skyrim-se' });
  });

  it('restores with a new name or null, and purges one or all', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-1', originalInstanceId: 'grounded-1', renamed: false });
    await restoreDeletedGameInstance('entry-1');
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_restore_deleted', {
      entryId: 'entry-1',
      name: null,
    });

    invokeMock.mockResolvedValueOnce({ instanceId: 'grounded-2', originalInstanceId: 'grounded-1', renamed: true });
    await restoreDeletedGameInstance('entry-1', 'Grounded Restored');
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_restore_deleted', {
      entryId: 'entry-1',
      name: 'Grounded Restored',
    });

    invokeMock.mockResolvedValueOnce({ purged: 1, failed: 0, kept: 0 });
    await expect(purgeDeletedGameInstances('skyrim-se', 'entry-1')).resolves.toEqual({ purged: 1, failed: 0, kept: 0 });
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_purge_deleted', { gameId: 'skyrim-se', entryId: 'entry-1' });

    invokeMock.mockResolvedValueOnce({ purged: 2, failed: 1, kept: 0 });
    await purgeDeletedGameInstances('skyrim-se');
    expect(invokeMock).toHaveBeenLastCalledWith('games_instance_purge_deleted', { gameId: 'skyrim-se', entryId: null });
  });

  it.each([
    ['a stop without its play time', 'games_instance_stop', { instanceId: 'grounded-1', processesEnded: 1 }],
    ['a deleted list with no retention window', 'games_instance_deleted_list', { entries: [] }],
    ['a deleted entry without a size', 'games_instance_deleted_list', { retentionDays: 30, entries: [{ entryId: 'e', instanceId: 'i', gameId: 'g', name: 'n', deletedAt: 'd', expiresAt: 'x' }] }],
    ['a purge count that is text', 'games_instance_purge_deleted', { purged: 'all' }],
  ])('rejects %s', async (_label, command, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    const call = command === 'games_instance_stop'
      ? stopGameInstance('grounded-1')
      : command === 'games_instance_deleted_list'
        ? listDeletedGameInstances('skyrim-se')
        : purgeDeletedGameInstances('skyrim-se');
    await expect(call).rejects.toThrow(`${command} did not return`);
  });
});

describe('events', () => {
  beforeEach(() => {
    listenMock.mockReset();
  });

  it('hands the progress and exit payloads to the page', async () => {
    const seen: unknown[] = [];
    listenMock.mockImplementation((_name: string, handler: (event: { payload: unknown }) => void) => {
      handler({ payload: { filesDone: 50, filesTotal: 100, bytesHashed: 1, bytesTotal: 2 } });
      return Promise.resolve(() => undefined);
    });
    await onCreateProgress((progress) => seen.push(progress));
    expect(listenMock).toHaveBeenCalledWith('games-instance-create-progress', expect.any(Function));
    expect(seen).toEqual([{ filesDone: 50, filesTotal: 100, bytesHashed: 1, bytesTotal: 2 }]);

    const exits: unknown[] = [];
    listenMock.mockImplementation((_name: string, handler: (event: { payload: unknown }) => void) => {
      handler({ payload: { instanceId: 'grounded-1', restoreError: 'hash mismatch' } });
      return Promise.resolve(() => undefined);
    });
    await onInstanceExited((exited) => exits.push(exited));
    expect(listenMock).toHaveBeenCalledWith('games-instance-exited', expect.any(Function));
    expect(exits).toEqual([{ instanceId: 'grounded-1', restoreError: 'hash mismatch' }]);
  });
});

describe('error helpers', () => {
  it('reads the message and code from a GamesError, and says so when there is neither', () => {
    const error = { code: 'instance_running', message: "'grounded-1' is running." };
    expect(gamesErrorMessage(error)).toBe("'grounded-1' is running.");
    expect(gamesErrorCode(error)).toBe('instance_running');
    expect(gamesErrorMessage(new Error('boom'))).toBe('boom');
    expect(gamesErrorMessage(undefined)).toBe('Something went wrong, and Agora did not say what.');
    expect(gamesErrorCode('plain string')).toBeNull();
  });
});
