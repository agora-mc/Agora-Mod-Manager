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
      notes: [{ message: 'A note from the sorter', refusesLaunch: false, plugins: ['SkyUI.esp'] }],
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

