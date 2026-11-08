import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';

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

describe('load order lists', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    replies = {};
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

