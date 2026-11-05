import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { ConfirmProvider } from '@/components/ui/confirm';
import { InstanceContentTab } from './InstanceContentTab';

/** Replies by command name. A function is called with the command's arguments. */
let replies: Record<string, (args: Record<string, unknown>) => unknown>;

function route() {
  invokeMock.mockImplementation(async (command: string, args: Record<string, unknown> = {}) => {
    const reply = replies[command];
    if (!reply) throw new Error(`unmocked command in a content test: ${command}`);
    return reply(args);
  });
}

const rowA = {
  position: 1,
  itemId: 'aaaa1111',
  layerId: 'content:aaaa1111',
  name: 'Odin Patch',
  sourceKind: 'archive',
  sourceLabel: 'Archive: odin.zip',
  enabled: true,
  mountPath: 'Data',
  sourcePath: '',
};

const rowB = {
  position: 2,
  itemId: 'bbbb2222',
  layerId: 'content:bbbb2222',
  name: 'Texture Pack',
  sourceKind: 'folder',
  sourceLabel: 'Folder: textures',
  enabled: false,
  mountPath: '',
  sourcePath: 'textures',
};

/** A row of `games_library_list`, tied to no game unless the test says otherwise. */
function libraryRow(itemId: string, name: string, gameIds: string[] = []) {
  return {
    itemId,
    name,
    description: null,
    version: null,
    gameId: gameIds.length === 1 ? gameIds[0] : null,
    gameIds,
    pageUrl: null,
    iconSha256: null,
    sourceKind: 'archive',
    sourceLabel: 'Archive: sky.7z',
    instances: [],
    fileCount: 4,
    totalSize: 2048,
    addedAtUnixMs: 1,
    pluginFiles: [],
    hasInstaller: false,
  };
}

const storeOther = libraryRow('cccc3333', 'Sky Mod');

function libraryReply(...items: unknown[]) {
  return { items, damaged: [], warnings: [] };
}

function mountTab() {
  return render(
    <ConfirmProvider>
      <InstanceContentTab gameId="skyrim-se" instanceId="inst-1" />
    </ConfirmProvider>,
  );
}

function addChoices(): string[] {
  const select = screen.getByLabelText<HTMLSelectElement>('Choose an item to add');
  return Array.from(select.options).map((option) => option.text).filter((text) => text !== 'Choose an item…');
}

describe('the "Add from your library" list', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    replies = {
      games_instance_content_list: () => ({ instanceId: 'inst-1', items: [] }),
    };
    route();
  });

  it('offers this instance’s game and untied items only, sorted by name', async () => {
    replies.games_library_list = () => libraryReply(
      libraryRow('m1', 'Sodium', ['minecraft']),
      libraryRow('s2', 'zEnhanced Camera', ['skyrim-se']),
      libraryRow('u1', 'My Textures'),
      libraryRow('s1', 'alternate Start', ['skyrim-se']),
      libraryRow('b1', 'Both Games', ['minecraft', 'skyrim-se']),
    );
    mountTab();
    await screen.findByLabelText('Choose an item to add');
    expect(addChoices()).toEqual(['alternate Start', 'Both Games', 'My Textures', 'zEnhanced Camera']);
  });

  it('shows no leak on the Minecraft side either: a game-only item stays out', async () => {
    replies.games_library_list = () => libraryReply(
      libraryRow('s1', 'SkyUI', ['skyrim-se']),
      libraryRow('m1', 'Sodium', ['minecraft']),
    );
    render(
      <ConfirmProvider>
        <InstanceContentTab gameId="minecraft" instanceId="inst-1" />
      </ConfirmProvider>,
    );
    await screen.findByLabelText('Choose an item to add');
    expect(addChoices()).toEqual(['Sodium']);
  });

  it('says the library has nothing for this game, not that it is empty or all added', async () => {
    replies.games_library_list = () => libraryReply(libraryRow('m1', 'Sodium', ['minecraft']));
    mountTab();
    expect(await screen.findByText(/Your library has nothing for this game yet/)).toBeInTheDocument();
    expect(screen.queryByLabelText('Choose an item to add')).toBeNull();
    expect(screen.queryByText(/already in this instance/)).toBeNull();
  });
});

describe('InstanceContentTab', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    replies = {
      games_instance_content_list: () => ({ instanceId: 'inst-1', items: [rowA, rowB] }),
      games_library_list: () => libraryReply(storeOther),
    };
    route();
  });

  it('lists the content in load order, with where each item came from and where it goes', async () => {
    mountTab();
    const rows = await screen.findAllByTestId('instance-content-row');
    expect(rows).toHaveLength(2);
    expect(within(rows[0]).getByText('Odin Patch')).toBeInTheDocument();
    expect(within(rows[0]).getByText('Archive: odin.zip')).toBeInTheDocument();
    expect(within(rows[0]).getByText('Installs into the Data folder')).toBeInTheDocument();
    expect(within(rows[1]).getByText('Off')).toBeInTheDocument();
    expect(within(rows[1]).getByText('Installs textures into the game folder')).toBeInTheDocument();
  });

  it('moves an item one place toward the top of the load order when asked to move it up', async () => {
    mountTab();
    const rows = await screen.findAllByTestId('instance-content-row');
    replies.games_instance_content_move = () => null;
    fireEvent.click(within(rows[0]).getByRole('button', { name: 'Move Odin Patch up (loads later)' }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('games_instance_content_move', {
      instanceId: 'inst-1',
      itemId: 'aaaa1111',
      position: 2,
    }));
  });

  it('shows an empty state with a next step when the instance has no content', async () => {
    replies.games_instance_content_list = () => ({ instanceId: 'inst-1', items: [] });
    mountTab();
    expect(await screen.findByText(/This instance has no content yet/)).toBeInTheDocument();
    expect(screen.queryByTestId('instance-content-row')).toBeNull();
  });

  it('says why the content cannot be read, rather than showing an empty tab', async () => {
    replies.games_instance_content_list = () => Promise.reject({
      code: 'core_failed',
      message: 'Layer file is damaged.',
    });
    mountTab();
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Layer file is damaged.');
    expect(screen.queryByText(/This instance has no content yet/)).toBeNull();
  });

  it('offers only library items the instance does not already have', async () => {
    mountTab();
    await screen.findAllByTestId('instance-content-row');
    const options = Array.from(screen.getByRole('combobox').querySelectorAll('option')).map((o) => o.textContent);
    expect(options).toEqual(['Choose an item…', 'Sky Mod']);
  });

  it('opens the installer when an added archive needs one, rather than adding it silently', async () => {
    replies.games_instance_content_add = () => ({
      status: 'needs_installer',
      itemId: 'cccc3333',
      reason: 'it has a fomod installer',
      topLevel: ['fomod'],
    });
    replies.games_fomod_show = () => ({
      installer: {
        itemId: 'cccc3333',
        title: 'Sky Installer',
        author: null,
        version: null,
        description: null,
        requires: null,
        alwaysInstalledCount: 0,
        conditionalInstallCount: 0,
        steps: [],
      },
      suggestedChoices: [],
    });
    mountTab();
    await screen.findAllByTestId('instance-content-row');
    fireEvent.change(screen.getByRole('combobox'), { target: { value: 'cccc3333' } });
    fireEvent.click(screen.getByRole('button', { name: 'Add to instance' }));
    expect(await screen.findByTestId('fomod-wizard')).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith('games_instance_content_add', {
      instanceId: 'inst-1',
      itemId: 'cccc3333',
    });
  });
});
