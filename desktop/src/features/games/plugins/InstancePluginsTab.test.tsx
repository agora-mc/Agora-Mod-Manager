import { beforeEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { InstancePluginsTab } from './index';

let replies: Record<string, (args: Record<string, unknown>) => unknown>;

function route() {
  invokeMock.mockImplementation(async (command: string, args: Record<string, unknown> = {}) => {
    const reply = replies[command];
    if (!reply) throw new Error(`unmocked command in a plugins test: ${command}`);
    return reply(args);
  });
}

function view(overrides: Record<string, unknown> = {}) {
  return {
    supported: true,
    listId: 'plugins',
    listName: 'Plugins',
    rulesKind: 'script',
    intro: 'Read the list from the top down.',
    itemHeading: 'Plugin',
    hasSorterUpdate: true,
    sorterUpdateLabel: 'Update masterlist',
    rulesApply: true,
    listExists: true,
    rows: [],
    findings: [],
    notes: [],
    notesOutOfDate: false,
    masterlist: null,
    hasOverlapsReader: false,
    overlaps: null,
    overlapsOutOfDate: false,
    ...overrides,
  };
}

const row = {
  position: 1,
  name: 'SkyUI.esp',
  implicit: false,
  active: true,
  present: true,
  managed: true,
  locked: false,
  badges: [],
  masters: [],
};

function mountTab() {
  return render(<InstancePluginsTab gameId="skyrim-se" instanceId="inst-1" />);
}

/** Lets the settings reads settle before the test asserts on what they changed. */
const flush = () => act(async () => { await new Promise((resolve) => setTimeout(resolve, 0)); });

describe('load order lists', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    replies = {};
    replies.get_setting = (args) =>
      args.key === 'network_plugins_enabled' ? false : args.key === 'network_lockdown_enabled' ? false : null;
    replies.set_setting = () => null;
    replies.games_plugins_rules = () => ({ rules: [] });
    route();
  });

  it('shows no load-order UI when the game keeps no list', async () => {
    replies.games_plugins_lists = () => ({ lists: [] });
    replies.games_plugins_list = () => view({ supported: false, rulesApply: false, listExists: false, listId: '', listName: '', rulesKind: '' });
    mountTab();
    expect(await screen.findByText(/does not keep a plugin list/)).toBeInTheDocument();
    expect(screen.queryByRole('tablist')).toBeNull();
    expect(screen.queryByRole('button', { name: 'Sort…' })).toBeNull();
  });

  it('keeps today’s single list, with Sort, and no tab row', async () => {
    replies.games_plugins_lists = () => ({ lists: [{ id: 'plugins', name: 'Plugins', rulesKind: 'script' }] });
    replies.games_plugins_list = () => view({ rows: [row] });
    mountTab();
    expect(await screen.findByRole('heading', { name: 'Plugins' })).toBeInTheDocument();
    expect(screen.queryByRole('tablist')).toBeNull();
    expect(screen.getByRole('button', { name: 'Sort…' })).toBeEnabled();
  });

  it('uses the list’s name and plain words for a game that is not Creation Engine', async () => {
    replies.games_plugins_lists = () => ({ lists: [{ id: 'mods', name: 'Mods', rulesKind: 'none' }] });
    replies.games_plugins_list = () => view({
      listId: 'mods',
      listName: 'Mods',
      rulesKind: 'none',
      intro: null,
      itemHeading: null,
      hasSorterUpdate: false,
      sorterUpdateLabel: null,
      rulesApply: false,
      rows: [{ ...row, name: 'my_mod', badges: [] }],
    });
    mountTab();
    expect(await screen.findByRole('heading', { name: 'Mods' })).toBeInTheDocument();
    expect(screen.getByText('The game reads these from the top of this list down. When two change the same thing, the lower one wins.')).toBeInTheDocument();
    expect(screen.getByRole('columnheader', { name: 'Name' })).toBeInTheDocument();
    expect(screen.queryByRole('columnheader', { name: 'Plugin' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Sort…' })).toBeNull();
    expect(screen.queryByRole('button', { name: /Update masterlist/ })).toBeNull();
  });

  it('shows the list before the sorter notes arrive, then fills them in', async () => {
    let resolveNotes!: (value: unknown) => void;
    const notes = new Promise((resolve) => {
      resolveNotes = resolve;
    });
    replies.games_plugins_lists = () => ({ lists: [{ id: 'plugins', name: 'Plugins', rulesKind: 'script' }] });
    replies.games_plugins_list = () => view({ rows: [row], notesOutOfDate: true });
    replies.games_plugins_sorter_notes = () => notes;
    mountTab();

    expect(await screen.findByText('SkyUI.esp')).toBeInTheDocument();
    expect(screen.queryAllByText('A note from the sorter')).toHaveLength(0);

    resolveNotes({
      notes: [{ message: 'A note from the sorter', refusesLaunch: false, level: 'warning', plugins: ['SkyUI.esp'] }],
      notesOutOfDate: false,
    });
    expect(await screen.findAllByText('A note from the sorter')).not.toHaveLength(0);
  });

  it('shows a tab per list and each list keeps its own order', async () => {
    replies.games_plugins_lists = () => ({
      lists: [
        { id: 'plugins', name: 'Plugins', rulesKind: 'script' },
        { id: 'redmods', name: 'REDmods', rulesKind: 'none' },
      ],
    });
    replies.games_plugins_list = (args) => args.listId === 'redmods'
      ? view({ listId: 'redmods', listName: 'REDmods', rulesKind: 'none', hasSorterUpdate: false, sorterUpdateLabel: null, rulesApply: false, rows: [{ ...row, name: 'my_mod' }] })
      : view({ rows: [row] });

    mountTab();
    const tabs = await screen.findByRole('tablist');
    expect(within(tabs).getAllByRole('tab').map((tab) => tab.textContent)).toEqual(['Plugins', 'REDmods']);
    expect(await screen.findByText('SkyUI.esp')).toBeInTheDocument();

    fireEvent.click(within(tabs).getByRole('tab', { name: 'REDmods' }));
    expect(await screen.findByText('my_mod')).toBeInTheDocument();
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('games_plugins_list', { instanceId: 'inst-1', listId: 'redmods' }));
    expect(screen.getByRole('heading', { name: 'Load order' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Sort…' })).toBeNull();
  });
});

describe('the network gate beside the sorter update', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    replies = {};
    replies.games_plugins_lists = () => ({ lists: [{ id: 'plugins', name: 'Plugins', rulesKind: 'script' }] });
    replies.games_plugins_list = () => view({ rows: [row] });
    replies.set_setting = () => null;
    replies.games_plugins_rules = () => ({ rules: [] });
    route();
  });

  const updateButton = () => screen.getByRole('button', { name: /Update masterlist/ });

  it('disables the update and offers the switch when the network is off', async () => {
    let resolveNetwork!: (value: unknown) => void;
    replies.get_setting = (args) =>
      args.key === 'network_plugins_enabled'
        ? new Promise((resolve) => {
            resolveNetwork = resolve;
          })
        : false;
    mountTab();
    await screen.findByText('SkyUI.esp');

    resolveNetwork(false);
    await flush();

    expect(updateButton()).toBeDisabled();
    expect(screen.getByText('Update masterlist needs plugins to reach the network, which is turned off.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Turn on network for plugins' })).toBeInTheDocument();
  });

  it('leaves the update as it was when the network is on', async () => {
    let resolveNetwork!: (value: unknown) => void;
    replies.get_setting = (args) =>
      args.key === 'network_plugins_enabled'
        ? new Promise((resolve) => {
            resolveNetwork = resolve;
          })
        : false;
    mountTab();
    await screen.findByText('SkyUI.esp');

    resolveNetwork(true);
    await flush();

    expect(updateButton()).toBeEnabled();
    expect(screen.queryByTestId('plugins-network-gate')).toBeNull();
  });

  it('disables the update and names Privacy when Lockdown Mode is on', async () => {
    replies.get_setting = (args) => (args.key === 'network_plugins_enabled' ? true : true);
    mountTab();
    await screen.findByText('SkyUI.esp');
    await flush();

    expect(updateButton()).toBeDisabled();
    expect(
      screen.getByText("Lockdown Mode is on, so plugins can't reach the network. Turn it off in Settings > Privacy to use Update masterlist."),
    ).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Turn on network for plugins' })).toBeNull();
  });

  it('keeps the update enabled when the network setting cannot be read', async () => {
    replies.get_setting = () => Promise.reject(new Error('the settings store is unavailable'));
    mountTab();
    await screen.findByText('SkyUI.esp');
    await flush();

    expect(updateButton()).toBeEnabled();
    expect(screen.queryByTestId('plugins-network-gate')).toBeNull();
  });

  it('turns the network on through the settings command and reports it', async () => {
    const saved: Array<{ key: unknown; value: unknown }> = [];
    replies.get_setting = (args) => (args.key === 'network_plugins_enabled' ? false : false);
    replies.set_setting = (args) => {
      saved.push({ key: args.key, value: args.value });
      return null;
    };
    mountTab();
    await screen.findByText('SkyUI.esp');
    await flush();
    expect(updateButton()).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: 'Turn on network for plugins' }));

    expect(await screen.findByRole('status')).toHaveTextContent(
      'Network for plugins is on. You can turn it off in Settings > Plugins.',
    );
    await waitFor(() => expect(updateButton()).toBeEnabled());
    expect(saved).toEqual([{ key: 'network_plugins_enabled', value: true }]);
    expect(screen.queryByRole('button', { name: 'Turn on network for plugins' })).toBeNull();
  });

  it('shows a failed save and changes nothing', async () => {
    replies.get_setting = (args) => (args.key === 'network_plugins_enabled' ? false : false);
    replies.set_setting = () => Promise.reject({ code: 'local_state_failed', message: 'The settings store is read-only.' });
    mountTab();
    await screen.findByText('SkyUI.esp');
    await flush();
    expect(updateButton()).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: 'Turn on network for plugins' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('The settings store is read-only.');
    expect(updateButton()).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Turn on network for plugins' })).toBeInTheDocument();
  });
});

describe('conflicts card and load order rules', () => {
  const heavy = { ...row, position: 1, name: 'Heavy Boots.esp' };
  const moon = { ...row, position: 2, name: 'Moon Jump.esp' };
  const rock = { ...row, position: 3, name: 'Rock.esp' };

  const pair = (
    winner: string,
    loser: string,
    count: number,
    labels: string[],
    newSinceLastLaunch = false,
  ) => ({ winner, loser, count, labels, newSinceLastLaunch });

  const report = (overrides: Record<string, unknown> = {}) => ({
    pairs: [pair('Heavy Boots.esp', 'Moon Jump.esp', 1, ['GMST fJumpHeightMin'], true)],
    patched: 2,
    decided: 0,
    implicitPairs: 1,
    notes: [],
    ...overrides,
  });

  const rule = (item: string, after: string, unused: string | null = null) => ({ item, after, unused });

  beforeEach(() => {
    invokeMock.mockReset();
    replies = {};
    replies.games_plugins_lists = () => ({ lists: [{ id: 'plugins', name: 'Plugins', rulesKind: 'script' }] });
    replies.games_plugins_list = () => view({ rows: [heavy, moon, rock] });
    replies.games_plugins_rules = () => ({ rules: [] });
    replies.get_setting = () => false;
    replies.set_setting = () => null;
    route();
  });

  it('says it is reading records the first time, and never calls that no conflicts', async () => {
    let resolveOverlaps!: (value: unknown) => void;
    replies.games_plugins_list = () => view({
      rows: [heavy, moon, rock],
      hasOverlapsReader: true,
      overlaps: null,
      overlapsOutOfDate: true,
    });
    replies.games_plugins_conflicts = () => new Promise((resolve) => { resolveOverlaps = resolve; });
    mountTab();

    const card = await screen.findByTestId('plugins-conflicts');
    expect(card).toHaveAttribute('data-state', 'reading');
    expect(within(card).getByText("Reading your plugins' records. The first time takes a few seconds.")).toBeInTheDocument();
    expect(within(card).queryByText(/No conflicts/)).toBeNull();

    resolveOverlaps(report());
    expect(await within(card).findByText(/Heavy Boots\.esp/)).toBeInTheDocument();
    expect(card).toHaveAttribute('data-state', 'pairs');
  });

  it('shows one muted line with the reason when the records cannot be read', async () => {
    replies.games_plugins_list = () => view({
      rows: [heavy, moon, rock],
      hasOverlapsReader: true,
      overlaps: null,
      overlapsOutOfDate: true,
    });
    replies.games_plugins_conflicts = () => Promise.reject({
      code: 'core_failed',
      message: 'the reader timed out',
    });
    mountTab();

    const card = await screen.findByTestId('plugins-conflicts');
    expect(await within(card).findByText('Could not check: the reader timed out')).toBeInTheDocument();
    expect(card).toHaveAttribute('data-state', 'failed');
    expect(within(card).queryByText(/No conflicts/)).toBeNull();
  });

  it('says no conflicts and what a patch and the game own files already handle', async () => {
    replies.games_plugins_list = () => view({
      rows: [heavy, moon, rock],
      hasOverlapsReader: true,
      overlaps: report({ pairs: [] }),
      overlapsOutOfDate: false,
    });
    mountTab();

    const card = await screen.findByTestId('plugins-conflicts');
    expect(card).toHaveAttribute('data-state', 'none');
    expect(within(card).getByText(/No conflicts\. 2 overlaps are handled by a patch\. 1 is between the game's own files\./)).toBeInTheDocument();
  });

  it('lists pairs with their records, a New badge, the footer, and a filter chip', async () => {
    replies.games_plugins_list = () => view({
      rows: [heavy, moon, rock],
      hasOverlapsReader: true,
      overlaps: report({
        pairs: [
          pair('Rock.esp', 'Moon Jump.esp', 3, ['GMST fJumpHeightMin', 'MISC RockWeight']),
          pair('Heavy Boots.esp', 'Moon Jump.esp', 1, ['GMST fJumpHeightMin'], true),
        ],
      }),
      overlapsOutOfDate: false,
    });
    mountTab();

    const card = await screen.findByTestId('plugins-conflicts');
    const pairs = within(card).getAllByTestId('conflict-pair');
    expect(pairs[0]).toHaveTextContent('Heavy Boots.esp wins over Moon Jump.esp · 1 record');
    expect(pairs[0]).toHaveTextContent('New');
    expect(pairs[1]).toHaveTextContent('Rock.esp wins over Moon Jump.esp · 3 records');
    expect(card).toHaveTextContent('2 pairs are handled by a patch.');
    expect(card).toHaveTextContent("1 is between the game's own files, which can't be reordered.");

    fireEvent.click(within(pairs[1]).getByRole('button', { name: /Show the records/ }));
    expect(within(pairs[1]).getByText('MISC RockWeight')).toBeInTheDocument();

    // The badge narrows the card and a chip clears it.
    const badge = screen.getAllByTestId('plugin-conflict-badge')[0];
    fireEvent.click(badge);
    expect(screen.getByRole('button', { name: /Showing Heavy Boots\.esp · Show all/ })).toBeInTheDocument();
    expect(within(card).getAllByTestId('conflict-pair')).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: /Showing Heavy Boots\.esp · Show all/ }));
    expect(within(card).getAllByTestId('conflict-pair')).toHaveLength(2);
  });

  it('keeps the order, says what happened, and removes the rule on Undo', async () => {
    const before = report();
    const after = report({ pairs: [], decided: 1 });
    replies.games_plugins_list = () => view({
      rows: [heavy, moon, rock],
      hasOverlapsReader: true,
      overlaps: before,
      overlapsOutOfDate: false,
    });
    replies.games_plugins_rules = () => ({ rules: [rule('Heavy Boots.esp', 'Moon Jump.esp')] });
    replies.games_plugins_conflict_action = (args) => {
      expect(args).toMatchObject({ winner: 'Heavy Boots.esp', loser: 'Moon Jump.esp', letLoserWin: false });
      return {
        rules: [rule('Heavy Boots.esp', 'Moon Jump.esp')],
        plugins: view({ rows: [heavy, moon, rock], hasOverlapsReader: true, overlaps: after, overlapsOutOfDate: false }),
        overlaps: after,
        overlapsError: null,
      };
    };
    replies.games_plugins_rule_remove = (args) => {
      expect(args).toMatchObject({ item: 'Heavy Boots.esp', after: 'Moon Jump.esp' });
      return { changed: true, rules: [], overlaps: before, overlapsError: null };
    };
    mountTab();

    const card = await screen.findByTestId('plugins-conflicts');
    fireEvent.click(within(card).getByRole('button', { name: 'Keep this order' }));

    const conflictNotice = await screen.findByTestId('plugins-conflict-notice');
    expect(conflictNotice).toHaveTextContent('Kept this order: Moon Jump.esp loads before Heavy Boots.esp.');
    fireEvent.click(within(conflictNotice).getByRole('button', { name: 'Undo' }));

    expect(await screen.findByRole('status')).toHaveTextContent(
      'Removed the rule that Heavy Boots.esp loads after Moon Jump.esp.',
    );
    expect(screen.queryByTestId('plugins-conflict-notice')).toBeNull();
  });

  it('lets the loser win with a rule and a move, and Undo moves it back', async () => {
    const before = report();
    const after = report({ pairs: [], decided: 1 });
    const movedRows = [
      { ...heavy, position: 1 },
      { ...moon, position: 2 },
      { ...rock, position: 3 },
    ];
    replies.games_plugins_list = () => view({
      rows: [{ ...moon, position: 1 }, { ...heavy, position: 2 }, rock],
      hasOverlapsReader: true,
      overlaps: before,
      overlapsOutOfDate: false,
    });
    replies.games_plugins_conflict_action = () => ({
      rules: [rule('Moon Jump.esp', 'Heavy Boots.esp')],
      plugins: view({ rows: movedRows, hasOverlapsReader: true, overlaps: after, overlapsOutOfDate: false }),
      overlaps: after,
      overlapsError: null,
    });
    replies.games_plugins_rule_remove = () => ({ changed: true, rules: [], overlaps: null, overlapsError: null });
    replies.games_plugins_move = (args) => {
      expect(args).toMatchObject({ plugin: 'Moon Jump.esp', to: 1, breakRule: false });
      return {
        report: { plugin: 'Moon Jump.esp', from: 2, to: 1, written: true },
        plugins: view({ rows: [{ ...moon, position: 1 }, { ...heavy, position: 2 }, rock] }),
      };
    };
    replies.games_plugins_conflicts = () => before;
    mountTab();

    const card = await screen.findByTestId('plugins-conflicts');
    fireEvent.click(within(card).getByRole('button', { name: /Let Moon Jump\.esp win/ }));

    const conflictNotice = await screen.findByTestId('plugins-conflict-notice');
    expect(conflictNotice).toHaveTextContent('Let Moon Jump.esp win. It now loads after Heavy Boots.esp.');
    fireEvent.click(within(conflictNotice).getByRole('button', { name: 'Undo' }));

    expect(await screen.findByRole('status')).toHaveTextContent('Put Moon Jump.esp back and removed the rule.');
    expect(invokeMock).toHaveBeenCalledWith('games_plugins_move', {
      instanceId: 'inst-1',
      plugin: 'Moon Jump.esp',
      to: 1,
      breakRule: false,
      listId: 'plugins',
    });
  });

  it('lists the rules, names the unused one, and removes a rule', async () => {
    const inUse = rule('Heavy Boots.esp', 'Moon Jump.esp');
    const unused = rule('Rock.esp', 'Gone.esp', 'Gone.esp');
    replies.games_plugins_rules = () => ({ rules: [inUse, unused] });
    replies.games_plugins_rule_remove = () => ({
      changed: true,
      rules: [unused],
      overlaps: null,
      overlapsError: null,
    });
    mountTab();

    const section = await screen.findByTestId('plugins-rules');
    const toggle = within(section).getByRole('button', { name: /Your load order rules \(2\)/ });
    expect(toggle).toHaveAttribute('aria-expanded', 'false');
    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute('aria-expanded', 'true');
    expect(within(section).getByText('Heavy Boots.esp loads after Moon Jump.esp')).toBeInTheDocument();
    expect(within(section).getByTestId('rule-unused')).toHaveTextContent(
      'Unused: Gone.esp is not in the list right now.',
    );

    fireEvent.click(within(section).getAllByRole('button', { name: 'Remove' })[0]);
    expect(await screen.findByRole('status')).toHaveTextContent(
      'Removed the rule that Heavy Boots.esp loads after Moon Jump.esp.',
    );
    expect(screen.queryByText('Heavy Boots.esp loads after Moon Jump.esp')).toBeNull();
  });

  it('asks before a move that breaks a rule, and only then removes it', async () => {
    let calls = 0;
    replies.games_plugins_move = (args) => {
      calls += 1;
      if (calls < 3) {
        expect(args).toMatchObject({ breakRule: false });
        return Promise.reject({
          code: 'BREAKS_RULE',
          message: "moving 'Moon Jump.esp' there breaks the rule that 'Heavy Boots.esp' loads after 'Moon Jump.esp'",
          details: { item: 'Heavy Boots.esp', after: 'Moon Jump.esp' },
        });
      }
      expect(args).toMatchObject({ breakRule: true });
      return {
        report: { plugin: 'Moon Jump.esp', from: 2, to: 1, written: true },
        plugins: view({ rows: [{ ...moon, position: 1 }, { ...heavy, position: 2 }, rock] }),
      };
    };
    mountTab();

    await screen.findByText('Moon Jump.esp');
    fireEvent.click(screen.getByRole('button', { name: 'Move Moon Jump.esp up' }));

    const dialog = await screen.findByRole('dialog', { name: 'Move Moon Jump.esp' });
    expect(dialog).toHaveTextContent("This breaks your rule 'Heavy Boots.esp loads after Moon Jump.esp'.");
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }));
    expect(calls).toBe(1);

    fireEvent.click(screen.getByRole('button', { name: 'Move Moon Jump.esp up' }));
    const again = await screen.findByRole('dialog', { name: 'Move Moon Jump.esp' });
    fireEvent.click(within(again).getByRole('button', { name: 'Move and remove the rule' }));
    expect(await screen.findByRole('status')).toHaveTextContent('Moon Jump.esp moved to position 1.');
    expect(calls).toBe(3);
  });

  it('tells the sort dialog how many rules it keeps', async () => {
    replies.games_plugins_rules = () => ({ rules: [rule('Heavy Boots.esp', 'Moon Jump.esp')] });
    replies.games_plugins_sort = () => ({
      report: { moves: [], blocked: [], written: false, mode: 'masters', message: null },
      plugins: view({ rows: [heavy, moon, rock] }),
    });
    mountTab();

    await screen.findByRole('button', { name: 'Sort…' });
    await waitFor(() => expect(screen.getByTestId('plugins-rules')).toBeInTheDocument());
    fireEvent.click(screen.getByRole('button', { name: 'Sort…' }));
    expect(await screen.findByRole('dialog', { name: 'Sort plugins' })).toHaveTextContent(
      'Your 1 load order rule is kept.',
    );
  });

  it('titles the findings card notes when nothing in it is a warning', async () => {
    replies.games_plugins_list = () => view({
      rows: [heavy],
      notes: [{ message: 'A note from the sorter', refusesLaunch: false, level: 'info', plugins: [] }],
    });
    mountTab();
    expect(await screen.findByRole('heading', { name: 'Notes on the load order' })).toBeInTheDocument();
    expect(screen.getByText('Info')).toBeInTheDocument();
  });

  it('keeps the problems title when a warning is present', async () => {
    replies.games_plugins_list = () => view({
      rows: [heavy],
      notes: [{ message: 'A note from the sorter', refusesLaunch: false, level: 'warning', plugins: [] }],
    });
    mountTab();
    expect(await screen.findByRole('heading', { name: 'Problems with the load order' })).toBeInTheDocument();
    expect(screen.getByText('Warning')).toBeInTheDocument();
  });
});
