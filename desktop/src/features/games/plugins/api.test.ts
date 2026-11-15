import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import {
  addLoadOrderRule,
  applyConflictAction,
  checkGamePlugins,
  describeFailure,
  listGamePlugins,
  listLoadOrderLists,
  listLoadOrderRules,
  moveGamePlugin,
  refreshOverlaps,
  removeLoadOrderRule,
  setGamePluginActive,
  setGamePluginLocked,
  sortGamePlugins,
  updateGameMasterlist,
  type PluginsView,
} from './api';

const row = {
  position: 1,
  name: 'SkyUI.esp',
  implicit: false,
  active: true,
  present: true,
  managed: false,
  locked: false,
  badges: ['Master'],
  masters: ['Skyrim.esm'],
};

const view: PluginsView = {
  supported: true,
  listId: 'plugins',
  listName: 'Plugins',
  rulesKind: 'script',
  intro: null,
  itemHeading: null,
  hasSorterUpdate: true,
  sorterUpdateLabel: 'Update masterlist',
  rulesApply: true,
  listExists: true,
  rows: [row],
  findings: [],
  notes: [],
  notesOutOfDate: false,
  masterlist: null,
  hasOverlapsReader: false,
  overlaps: null,
  overlapsOutOfDate: false,
};

const overlaps = {
  pairs: [{
    winner: 'Heavy Boots.esp',
    loser: 'Moon Jump.esp',
    count: 1,
    labels: ['GMST fJumpHeightMin'],
    newSinceLastLaunch: true,
  }],
  patched: 1,
  decided: 0,
  implicitPairs: 2,
  notes: [],
};

describe('load order lists', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('reads every list the game declares', async () => {
    const lists = { lists: [{ id: 'mods', name: 'Mods', rulesKind: 'none' }] };
    invokeMock.mockResolvedValueOnce(lists);
    await expect(listLoadOrderLists('inst-1')).resolves.toEqual(lists);
    expect(invokeMock).toHaveBeenCalledWith('games_plugins_lists', { instanceId: 'inst-1' });
  });

  it('rejects a reply that is not a list of lists', async () => {
    invokeMock.mockResolvedValueOnce({ lists: [{ id: 'x' }] });
    await expect(listLoadOrderLists('inst-1')).rejects.toThrow('games_plugins_lists did not return the expected plugin data.');
  });
});

describe('plugin commands', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('lists the plugins with the arguments the backend takes', async () => {
    invokeMock.mockResolvedValueOnce(view);
    await expect(listGamePlugins('inst-1')).resolves.toEqual(view);
    expect(invokeMock).toHaveBeenCalledWith('games_plugins_list', { instanceId: 'inst-1', listId: null });
  });

  it('passes the 1-based target position as `to`, the name the backend declares', async () => {
    invokeMock.mockResolvedValueOnce({
      report: { plugin: 'SkyUI.esp', from: 3, to: 2, written: true },
      plugins: view,
    });
    await moveGamePlugin('inst-1', 'SkyUI.esp', 2);
    expect(invokeMock).toHaveBeenCalledWith('games_plugins_move', {
      instanceId: 'inst-1',
      plugin: 'SkyUI.esp',
      to: 2,
      breakRule: false,
      listId: null,
    });
  });

  it('asks the backend to remove a rule only when the user agreed to break it', async () => {
    invokeMock.mockResolvedValueOnce({
      report: { plugin: 'SkyUI.esp', from: 3, to: 2, written: true },
      plugins: view,
    });
    await moveGamePlugin('inst-1', 'SkyUI.esp', 2, undefined, true);
    expect(invokeMock).toHaveBeenCalledWith('games_plugins_move', {
      instanceId: 'inst-1',
      plugin: 'SkyUI.esp',
      to: 2,
      breakRule: true,
      listId: null,
    });
  });

  it('sends a sort as a dry run or for real, as it is asked', async () => {
    invokeMock.mockResolvedValueOnce({ report: { moves: [], blocked: [], written: false, mode: 'sorter', message: null }, plugins: view });
    await sortGamePlugins('inst-1', true);
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_sort', { instanceId: 'inst-1', dryRun: true, listId: null });
  });

  it('downloads the masterlist and reads its date back', async () => {
    const masterlist = { fetchedAt: '2026-01-02T03:04:05Z', source: 'https://api.github.com/repos/loot/skyrimse' };
    invokeMock.mockResolvedValueOnce(masterlist);
    await expect(updateGameMasterlist('inst-1')).resolves.toEqual(masterlist);
    expect(invokeMock).toHaveBeenCalledWith('games_plugins_update_masterlist', { instanceId: 'inst-1', listId: null });
  });

  it('sends the lock and active flags as they are asked', async () => {
    invokeMock.mockResolvedValue({ changed: true, plugins: view });
    await setGamePluginActive('inst-1', 'SkyUI.esp', false);
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_set_active', {
      instanceId: 'inst-1',
      plugin: 'SkyUI.esp',
      active: false,
      listId: null,
    });
    await setGamePluginLocked('inst-1', 'SkyUI.esp', true);
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_set_locked', {
      instanceId: 'inst-1',
      plugin: 'SkyUI.esp',
      locked: true,
      listId: null,
    });
  });

  it('accepts a change that was saved when only the re-read failed, and never as a clean list', async () => {
    invokeMock.mockResolvedValueOnce({ changed: true, plugins: null, reloadError: 'The list could not be read.' });
    await expect(setGamePluginActive('inst-1', 'SkyUI.esp', false)).resolves.toMatchObject({
      changed: true,
      plugins: null,
      reloadError: 'The list could not be read.',
    });

    invokeMock.mockResolvedValueOnce({ changed: true, plugins: null });
    await expect(setGamePluginActive('inst-1', 'SkyUI.esp', false)).rejects.toThrow(
      'games_plugins_set_active did not return the expected plugin data.',
    );
  });

  it('reads a check into its status and findings', async () => {
    invokeMock.mockResolvedValueOnce({
      status: 'findings',
      findings: [{ message: 'Skyrim.esm is missing', refusesLaunch: true, level: 'finding', plugins: ['SkyUI.esp'] }],
    });
    await expect(checkGamePlugins('inst-1')).resolves.toMatchObject({ status: 'findings' });
  });

  it('reads conflicts, rules and the two actions with the arguments the backend takes', async () => {
    invokeMock.mockResolvedValueOnce(overlaps);
    await expect(refreshOverlaps('inst-1')).resolves.toEqual(overlaps);
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_conflicts', { instanceId: 'inst-1', listId: null });

    const rules = [{ item: 'Heavy Boots.esp', after: 'Moon Jump.esp', unused: null }];
    invokeMock.mockResolvedValueOnce({ rules });
    await expect(listLoadOrderRules('inst-1')).resolves.toEqual(rules);
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_rules', { instanceId: 'inst-1', listId: null });

    invokeMock.mockResolvedValueOnce({ changed: true, rules, overlaps, overlapsError: null });
    await expect(addLoadOrderRule('inst-1', 'Heavy Boots.esp', 'Moon Jump.esp')).resolves.toMatchObject({ changed: true });
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_rule_add', {
      instanceId: 'inst-1',
      item: 'Heavy Boots.esp',
      after: 'Moon Jump.esp',
      listId: null,
    });

    invokeMock.mockResolvedValueOnce({ changed: true, rules: [], overlaps, overlapsError: null });
    await expect(removeLoadOrderRule('inst-1', 'Heavy Boots.esp', 'Moon Jump.esp')).resolves.toMatchObject({ changed: true });
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_rule_remove', {
      instanceId: 'inst-1',
      item: 'Heavy Boots.esp',
      after: 'Moon Jump.esp',
      listId: null,
    });

    invokeMock.mockResolvedValueOnce({ rules, plugins: view, overlaps, overlapsError: null });
    await expect(applyConflictAction('inst-1', 'Heavy Boots.esp', 'Moon Jump.esp', true)).resolves.toMatchObject({ plugins: view });
    expect(invokeMock).toHaveBeenLastCalledWith('games_plugins_conflict_action', {
      instanceId: 'inst-1',
      winner: 'Heavy Boots.esp',
      loser: 'Moon Jump.esp',
      letLoserWin: true,
      listId: null,
    });
  });

  it('rejects a conflicts reply that is not a report, never reading it as "no conflicts"', async () => {
    invokeMock.mockResolvedValueOnce(null);
    await expect(refreshOverlaps('inst-1')).rejects.toThrow('games_plugins_conflicts did not return');
  });

  it.each([
    ['null (an unmocked command answers this)', null],
    ['a bare array', []],
    ['a list with no supported flag', { rows: [], findings: [] }],
    ['rows that are not a list', { ...view, rows: 'SkyUI.esp' }],
    ['a row with no name', { ...view, rows: [{ ...row, name: undefined }] }],
    ['a row whose active flag is a string', { ...view, rows: [{ ...row, active: 'yes' }] }],
    ['a finding with no plugin list', { ...view, findings: [{ message: 'x', refusesLaunch: false }] }],
  ])('rejects %s rather than reading it as an empty list', async (_label, reply) => {
    invokeMock.mockResolvedValueOnce(reply);
    await expect(listGamePlugins('inst-1')).rejects.toThrow('games_plugins_list did not return the expected plugin data.');
  });

  it('rejects a sort that does not say what it used', async () => {
    invokeMock.mockResolvedValueOnce({ report: { moves: [], blocked: [], written: false }, plugins: view });
    await expect(sortGamePlugins('inst-1', true)).rejects.toThrow('games_plugins_sort did not return');
  });

  it('rejects a check whose status is not one the tab knows', async () => {
    invokeMock.mockResolvedValueOnce({ status: 'fine', findings: [] });
    await expect(checkGamePlugins('inst-1')).rejects.toThrow('games_plugins_check did not return');
  });

  it('rejects a move that does not say where the plugin went', async () => {
    invokeMock.mockResolvedValueOnce({ report: { plugin: 'SkyUI.esp' }, plugins: view });
    await expect(moveGamePlugin('inst-1', 'SkyUI.esp', 2)).rejects.toThrow('games_plugins_move did not return');
  });

  it('lets a backend failure through unchanged', async () => {
    invokeMock.mockRejectedValueOnce({ code: 'core_failed', message: 'close the game', coreCode: 'ERR_X' });
    await expect(setGamePluginActive('inst-1', 'SkyUI.esp', true)).rejects.toMatchObject({ code: 'core_failed' });
  });
});

describe('describeFailure', () => {
  it('shows the backend message for a GamesError', () => {
    expect(describeFailure({ code: 'core_failed', message: 'close the game first' })).toBe('close the game first');
  });

  it.each([
    ['an Error', new Error('boom')],
    ['null', null],
    ['a message with no code', { message: 'boom' }],
  ])('falls back to a plain sentence for %s', (_label, failure) => {
    expect(describeFailure(failure)).toBe('Agora could not finish that. Try again.');
  });
});
