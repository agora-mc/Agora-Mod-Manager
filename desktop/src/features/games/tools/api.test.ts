import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import {
  addUserTool,
  diffGameTool,
  findUserTools,
  gameToolSwapCosts,
  gameToolSwapStatus,
  isRunResult,
  isSwapStatus,
  isToolsView,
  isUserToolsView,
  listGameTools,
  listUserTools,
  removeGameTool,
  removeUserTool,
  restoreGameToolSwap,
  runGameTool,
} from './api';

const row = {
  toolId: 'nemesis',
  name: 'Nemesis',
  declared: true,
  current: '2',
  previous: '1',
  status: 'current',
  conflicts: false,
  outputKind: 'generated_layer',
  outputFolder: null,
  installed: true,
  exe: 'Data/Nemesis_Engine/Nemesis Unlimited Behavior Engine.exe',
  fromMod: 'Nemesis Unlimited Behavior Engine',
  runOnly: false,
};

const costs = {
  installDir: 'D:/Games/Skyrim',
  dataFiles: 10,
  dataBytes: 1024,
  sameVolume: true,
  copyKind: 'kept',
  copyFolder: 'D:/Agora/swap',
  keptCopyExists: false,
  copyFiles: 10,
  copyBytes: 1024,
  freeBytes: null,
  steamRunning: false,
};

const outcome = {
  toolId: 'nemesis',
  name: 'Nemesis',
  run: '0000000000001-abcd1234',
  capture: 'vfs',
  captureReason: null,
  promoted: true,
  current: '2',
  previous: '1',
  written: ['Data/meshes/a.hkx'],
  deleted: [],
  exitCode: 0,
  cancelled: false,
  failedFolder: null,
  failure: null,
  copiedLayer: null,
  installChanges: { created: [], changed: [], deleted: [], mayBeIncomplete: false, unavailable: null },
  swap: null,
  offers: [],
  unchangedDropped: 0,
};

describe('listGameTools', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('returns the tools the backend lists, for the instance asked about', async () => {
    const view = { instanceId: 'sk-1', tools: [row], known: ['Nemesis', 'BodySlide'] };
    invokeMock.mockResolvedValueOnce(view);
    await expect(listGameTools('sk-1')).resolves.toEqual(view);
    expect(invokeMock).toHaveBeenCalledWith('games_tools_list', { instanceId: 'sk-1' });
  });

  it('accepts an empty tool list: an instance with no tools installed', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'sk-1', tools: [], known: [] });
    await expect(listGameTools('sk-1')).resolves.toEqual({ instanceId: 'sk-1', tools: [], known: [] });
  });

  it.each([
    ['null (an unmocked command answers this)', null],
    ['a bare array', []],
    ['a view without tools', { instanceId: 'sk-1', known: [] }],
    ['a view without the known tools', { instanceId: 'sk-1', tools: [] }],
    ['a row that does not say whether it is installed', { instanceId: 'sk-1', known: [], tools: [{ ...row, installed: undefined }] }],
    ['a row whose mod is not text', { instanceId: 'sk-1', known: [], tools: [{ ...row, fromMod: 3 }] }],
    ['a row without a tool id', { instanceId: 'sk-1', known: [], tools: [{ ...row, toolId: undefined }] }],
    ['a row with a status it does not know', { instanceId: 'sk-1', known: [], tools: [{ ...row, status: 'maybe' }] }],
    ['a row whose conflict flag is missing', { instanceId: 'sk-1', known: [], tools: [{ ...row, conflicts: undefined }] }],
  ])('rejects %s rather than showing no tools', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(listGameTools('sk-1')).rejects.toThrow('games_tools_list did not return the reply the screen expects.');
  });

  it('rejects when the command itself fails, with its message', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'core_failed', message: 'Instance was not found.' });
    await expect(listGameTools('sk-1')).rejects.toMatchObject({ code: 'core_failed' });
  });
});

describe('runGameTool', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('sends the capture and never confirms a swap unless asked to', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'finished', outcome });
    await runGameTool('sk-1', 'nemesis', 'auto');
    expect(invokeMock).toHaveBeenLastCalledWith('games_tools_run', {
      instanceId: 'sk-1',
      toolId: 'nemesis',
      capture: 'auto',
      refusal: null,
      confirmSwap: false,
    });
  });

  it('passes the refusal a fallback follows, and the swap confirmation', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'finished', outcome });
    await runGameTool('sk-1', 'nemesis', 'swap', { refusal: 'injection failed', confirmSwap: true });
    expect(invokeMock).toHaveBeenLastCalledWith('games_tools_run', {
      instanceId: 'sk-1',
      toolId: 'nemesis',
      capture: 'swap',
      refusal: 'injection failed',
      confirmSwap: true,
    });
  });

  it('accepts a refusal as a question, with its next tier', async () => {
    const refused = { status: 'vfsRefused', reason: 'injection failed', next: 'links', offers: [] };
    invokeMock.mockResolvedValueOnce(refused);
    await expect(runGameTool('sk-1', 'nemesis', 'auto')).resolves.toEqual(refused);
  });

  it('accepts a needs-swap answer', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'needsSwap', tool: 'bodyslide' });
    await expect(runGameTool('sk-1', 'bodyslide', 'auto')).resolves.toEqual({ status: 'needsSwap', tool: 'bodyslide' });
  });

  it.each([
    ['null', null],
    ['a finished run with no outcome', { status: 'finished' }],
    ['an outcome missing its install changes', { status: 'finished', outcome: { ...outcome, installChanges: undefined } }],
    ['a capture the screen does not know', { status: 'finished', outcome: { ...outcome, capture: 'copy' } }],
    ['a refusal with no reason', { status: 'vfsRefused', next: null, offers: [] }],
    ['an unknown status', { status: 'promoted', outcome }],
  ])('rejects %s rather than reading it as a finished run', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(runGameTool('sk-1', 'nemesis', 'auto')).rejects.toThrow('games_tools_run did not return the reply the screen expects.');
  });

  it('shows a failed run as the error it is', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'invalid_request', message: 'capture must be auto, vfs, links or swap, not \'x\'.' });
    await expect(runGameTool('sk-1', 'nemesis', 'auto')).rejects.toMatchObject({ code: 'invalid_request' });
  });
});

describe('the other tool commands', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('removes a tool and returns the view that results', async () => {
    const reply = { removedGenerations: ['1', '2'], view: { instanceId: 'sk-1', tools: [], known: [] } };
    invokeMock.mockResolvedValueOnce(reply);
    await expect(removeGameTool('sk-1', 'nemesis')).resolves.toEqual(reply);
  });

  it('rejects a remove reply without the view', async () => {
    invokeMock.mockResolvedValueOnce({ removedGenerations: [] });
    await expect(removeGameTool('sk-1', 'nemesis')).rejects.toThrow('games_tools_remove');
  });

  it('reads a diff, and rejects one without its lists', async () => {
    const diff = { toolId: 'nemesis', previous: '1', current: '2', added: ['a'], changed: [], removed: [] };
    invokeMock.mockResolvedValueOnce(diff);
    await expect(diffGameTool('sk-1', 'nemesis')).resolves.toEqual(diff);
    invokeMock.mockResolvedValueOnce({ toolId: 'nemesis', previous: '1', current: '2' });
    await expect(diffGameTool('sk-1', 'nemesis')).rejects.toThrow('games_tools_diff');
  });

  it('reads the swap costs with the free space unknown as null, not zero', async () => {
    invokeMock.mockResolvedValueOnce(costs);
    await expect(gameToolSwapCosts('sk-1')).resolves.toMatchObject({ freeBytes: null, copyKind: 'kept' });
    invokeMock.mockResolvedValueOnce({ ...costs, freeBytes: undefined });
    await expect(gameToolSwapCosts('sk-1')).rejects.toThrow('games_tools_swap_costs');
  });

  it('reads an interrupted swap, and no swap as null', async () => {
    invokeMock.mockResolvedValueOnce({ interrupted: null });
    await expect(gameToolSwapStatus('sk-1')).resolves.toEqual({ interrupted: null });
    invokeMock.mockResolvedValueOnce({ interrupted: { tool: null, running: false, unreadable: 'bad json' } });
    await expect(gameToolSwapStatus('sk-1')).resolves.toMatchObject({ interrupted: { tool: null, unreadable: 'bad json' } });
  });

  it('rejects a swap status of undefined rather than reading it as no swap', async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await expect(gameToolSwapStatus('sk-1')).rejects.toThrow('games_tools_swap_status');
  });

  it('reports what a restore put back', async () => {
    invokeMock.mockResolvedValueOnce({ restoredTools: ['bodyslide'] });
    await expect(restoreGameToolSwap('sk-1')).resolves.toEqual({ restoredTools: ['bodyslide'] });
  });
});

const userTool = {
  id: 'tool-1',
  toolId: 'user-tool1',
  name: 'FNIS',
  exe: 'Data/tools/FNIS.exe',
  args: ['-a'],
  knownToolId: null,
  outputIsMod: false,
  outputFolder: null,
  missing: false,
  keepsOutput: false,
};

const candidate = {
  exe: 'Data/tools/FNIS.exe',
  modId: 'aaaa1111',
  modName: 'FNIS',
  knownToolId: null,
  suggestedName: 'FNIS',
  alreadyAdded: false,
};

describe('the user tool commands', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('lists the instance’s own tools', async () => {
    const view = { instanceId: 'sk-1', tools: [userTool] };
    invokeMock.mockResolvedValueOnce(view);
    await expect(listUserTools('sk-1')).resolves.toEqual(view);
    expect(invokeMock).toHaveBeenCalledWith('games_user_tools_list', { instanceId: 'sk-1' });
  });

  it('rejects a malformed list rather than showing it as no tools', async () => {
    invokeMock.mockResolvedValueOnce({ instanceId: 'sk-1', tools: [{ ...userTool, args: 'none' }] });
    await expect(listUserTools('sk-1')).rejects.toThrow('games_user_tools_list');
    invokeMock.mockResolvedValueOnce(undefined);
    await expect(listUserTools('sk-1')).rejects.toThrow('games_user_tools_list');
  });

  it('adds a tool and reads the row back', async () => {
    invokeMock.mockResolvedValueOnce(userTool);
    await expect(addUserTool('sk-1', { name: 'FNIS', exe: 'Data/tools/FNIS.exe', args: ['-a'] })).resolves.toEqual(userTool);
    expect(invokeMock).toHaveBeenCalledWith('games_user_tools_add', {
      instanceId: 'sk-1',
      name: 'FNIS',
      exe: 'Data/tools/FNIS.exe',
      args: ['-a'],
      outputIsMod: false,
      outputFolder: null,
      keepsOutput: false,
    });
  });

  it('rejects an add reply that is not a tool', async () => {
    invokeMock.mockResolvedValueOnce({ id: 'tool-1' });
    await expect(addUserTool('sk-1', { name: 'FNIS', exe: 'x.exe', args: [] })).rejects.toThrow('games_user_tools_add');
  });

  it('removes a tool and reads the list that results', async () => {
    const view = { instanceId: 'sk-1', tools: [] };
    invokeMock.mockResolvedValueOnce(view);
    await expect(removeUserTool('sk-1', 'tool-1')).resolves.toEqual(view);
    expect(invokeMock).toHaveBeenCalledWith('games_user_tools_remove', { instanceId: 'sk-1', id: 'tool-1' });
  });

  it('reads the candidates, and rejects a reply that is not a list of them', async () => {
    invokeMock.mockResolvedValueOnce([candidate]);
    await expect(findUserTools('sk-1')).resolves.toEqual([candidate]);
    invokeMock.mockResolvedValueOnce({ candidates: [candidate] });
    await expect(findUserTools('sk-1')).rejects.toThrow('games_user_tools_find');
    invokeMock.mockResolvedValueOnce([{ ...candidate, alreadyAdded: 'no' }]);
    await expect(findUserTools('sk-1')).rejects.toThrow('games_user_tools_find');
  });
});

describe('the reply guards', () => {  it('accept the offers a run can carry, and nothing else', () => {
    expect(isRunResult({ status: 'vfsRefused', reason: 'x', next: 'swap', offers: [{ offer: 'copySwap', costs }] })).toBe(true);
    expect(isRunResult({ status: 'vfsRefused', reason: 'x', next: null, offers: [{ offer: 'bribe' }] })).toBe(false);
  });

  it('accept an interrupted swap with its tool unknown', () => {
    expect(isSwapStatus({ interrupted: { tool: null, running: true, unreadable: null } })).toBe(true);
    expect(isSwapStatus({ interrupted: 'yes' })).toBe(false);
  });

  it('refuse a view whose tools are not a list', () => {
    expect(isToolsView({ instanceId: 'sk-1', tools: 'nemesis' })).toBe(false);
    expect(isUserToolsView({ instanceId: 'sk-1', tools: 'nemesis' })).toBe(false);
    expect(isUserToolsView({ instanceId: 'sk-1', tools: [userTool] })).toBe(true);
    expect(isUserToolsView({ instanceId: 'sk-1', tools: [{ ...userTool, missing: 'no' }] })).toBe(false);
  });
});
