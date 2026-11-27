import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
// New downloads have their own tests; here the section stays empty.
vi.mock('@/features/downloads/useNewDownloads', () => ({
  useNewDownloads: () => ({ rows: [], count: 0, error: null, reload: async () => undefined }),
}));

import { LibraryPage } from './LibraryPage';
import { ConfirmProvider } from '@/components/ui/confirm';

const first = {
  itemId: 'a'.repeat(64),
  name: 'First item',
  description: 'A saved description.',
  version: '1.0.0',
  gameId: 'minecraft',
  gameIds: ['minecraft'],
  pageUrl: null,
  iconSha256: null,
  sourceKind: 'file',
  sourceLabel: 'File: first.jar',
  catalogStrategy: null,
  instances: [],
  fileCount: 1,
  totalSize: 1024,
  addedAtUnixMs: 1,
  pluginFiles: [] as string[],
  hasInstaller: false,
};

const second = {
  ...first,
  itemId: 'b'.repeat(64),
  name: 'Second item',
  description: null,
  version: '2.0.0',
  sourceLabel: 'Archive: second.zip',
};

const skyrimItem = {
  ...first,
  itemId: 'c'.repeat(64),
  name: 'Lux',
  description: 'A lighting overhaul.',
  gameId: 'skyrim-se',
  gameIds: ['skyrim-se'],
  sourceKind: 'archive',
  sourceLabel: 'Archive: lux.7z',
  pluginFiles: ['Lux.esp', 'Lux - Orbis.esp'],
  hasInstaller: true,
};

const untied = {
  ...first,
  itemId: 'd'.repeat(64),
  name: 'Odin Patch',
  description: null,
  gameId: null,
  gameIds: [] as string[],
  sourceKind: 'archive',
  sourceLabel: 'Archive: odin.zip',
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

function route() {
  invokeMock.mockImplementation(async (command: string) => {
    switch (command) {
      case 'games_library_list':
        return { items: [first, second, skyrimItem, untied], damaged: [], warnings: [] };
      case 'games_library_instances':
        return {
          instances: [{ instanceId: 'mc1', name: 'Minecraft One', gameId: 'minecraft' }],
          warnings: [],
        };
      case 'games_library_unused':
        return { items: [], totalSize: 0, warnings: [] };
      case 'games_library_icon':
        return null;
      case 'minecraft_add_stored_content':
        return installedMod;
      case 'games_instance_content_add':
        return { status: 'added' };
      default:
        throw new Error(`unmocked command in a library test: ${command}`);
    }
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  route();
});

describe('LibraryPage', () => {
  it('shows saved descriptions and says when none was saved', async () => {
    render(<LibraryPage />);
    expect(await screen.findByText('A saved description.')).toBeInTheDocument();
    expect(screen.getByText('No description was saved.')).toBeInTheDocument();
    expect(screen.getByText(/File: first.jar/)).toBeInTheDocument();
  });

  it('filters the list as the user searches', async () => {
    render(<LibraryPage />);
    await screen.findByText('A saved description.');
    fireEvent.change(screen.getByLabelText('Search your library'), {
      target: { value: 'second.zip' },
    });
    await waitFor(() => {
      expect(screen.queryByText('First item')).not.toBeInTheDocument();
      expect(screen.getByText('Second item')).toBeInTheDocument();
    });
  });

  it('loads a saved icon by its hash when the row renders', async () => {
    const iconHash = 'c'.repeat(64);
    const iconDataUrl = 'data:image/png;base64,AAAA';
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'games_library_list') {
        return {
          items: [{ ...first, iconSha256: iconHash }],
          damaged: [],
          warnings: [],
        };
      }
      if (command === 'games_library_instances') {
        return { instances: [], warnings: [] };
      }
      if (command === 'games_library_unused') {
        return { items: [], totalSize: 0, warnings: [] };
      }
      if (command === 'games_library_icon') return iconDataUrl;
      throw new Error(`unmocked command in a library test: ${command}`);
    });
    const { container } = render(<LibraryPage />);
    await screen.findByText('A saved description.');
    await waitFor(() => {
      expect(container.querySelector(`img[src="${iconDataUrl}"]`)).toBeTruthy();
    });
    expect(invokeMock).toHaveBeenCalledWith('games_library_icon', { sha256: iconHash });
  });

  it('adds a Minecraft item to a Minecraft instance and says so', async () => {
    render(<LibraryPage />);
    await screen.findByText('A saved description.');
    const rows = screen.getAllByTestId('library-item');
    fireEvent.click(within(rows[0]).getByRole('button', { name: /Add to instance/ }));

    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Add to instance' }));

    expect(await screen.findByRole('status')).toHaveTextContent(
      'Added First item to Minecraft One.',
    );
    expect(invokeMock).toHaveBeenCalledWith('minecraft_add_stored_content', {
      instanceId: 'mc1',
      itemId: first.itemId,
    });
  });

  it('shows the error the Minecraft command returns', async () => {
    const base = invokeMock.getMockImplementation()!;
    invokeMock.mockImplementation(async (command: string, args: unknown) => {
      if (command === 'minecraft_add_stored_content') {
        throw { code: 'ERR_CONTENT_ITEM_MISSING', message: 'The store item is missing.' };
      }
      return base(command, args);
    });

    render(<LibraryPage />);
    await screen.findByText('A saved description.');
    const rows = screen.getAllByTestId('library-item');
    fireEvent.click(within(rows[0]).getByRole('button', { name: /Add to instance/ }));

    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Add to instance' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('The store item is missing.');
  });

  it('says what removing an item reclaimed, and why when nothing was', async () => {
    // What the removal's collection answered, in order: one that freed objects, then one that
    // freed nothing and said why (MASTER_SPEC §32.8).
    const reports = [
      { summary: 'reclaimed 2 object(s)', reclaimed: 2, refused: false },
      {
        summary: 'nothing was reclaimed: a snapshot manifest cannot be read',
        reclaimed: 0,
        refused: true,
      },
    ];
    const base = invokeMock.getMockImplementation()!;
    invokeMock.mockImplementation(async (command: string, args: unknown) => {
      if (command === 'games_content_remove') return reports.shift();
      return base(command, args);
    });

    render(<ConfirmProvider><LibraryPage /></ConfirmProvider>);
    await screen.findByText('A saved description.');

    await userEvent.click(screen.getByRole('button', { name: 'More for First item' }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /Remove from library/ }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Remove' }));
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveTextContent(
        'Removed First item. Reclaimed 2 object(s).',
      );
    });

    await userEvent.click(screen.getByRole('button', { name: 'More for Second item' }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /Remove from library/ }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Remove' }));
    await waitFor(() => {
      expect(screen.getByRole('status')).toHaveTextContent(
        'Removed Second item. Nothing was reclaimed: a snapshot manifest cannot be read.',
      );
    });
  });

  it('removes only the unused items the user selects and confirms', async () => {
    const unusedItem = {
      itemId: 'e'.repeat(64),
      name: 'Old Mod',
      totalSize: 2 * 1024 * 1024,
      gameId: 'minecraft',
      gameIds: ['minecraft'],
    };
    const blockedItem = { ...unusedItem, itemId: 'a'.repeat(64), name: 'Blocked Mod' };
    const ignoredItem = { ...unusedItem, itemId: 'b'.repeat(64), name: 'Unselected Mod' };
    let remaining = [unusedItem, blockedItem, ignoredItem];
    invokeMock.mockImplementation(async (command: string, args?: { itemId: string }) => {
      switch (command) {
        case 'games_library_list':
          return { items: [first], damaged: [], warnings: [] };
        case 'games_library_instances':
          return { instances: [], warnings: [] };
        case 'games_library_unused':
          return { items: remaining, totalSize: remaining.length * unusedItem.totalSize, warnings: [] };
        case 'games_content_remove':
          remaining = remaining.filter((item) => item.itemId !== args?.itemId);
          return args?.itemId === blockedItem.itemId
            ? { summary: 'nothing was reclaimed: a linked object root', reclaimed: 0, refused: true }
            : { summary: 'reclaimed 2 object(s)', reclaimed: 2, refused: false };
        default:
          throw new Error(`unmocked command in a library test: ${command}`);
      }
    });

    render(<LibraryPage />);
    expect(await screen.findByTestId('unused-summary')).toHaveTextContent(
      'Unused: 3 items, 6.0 MB',
    );

    fireEvent.click(screen.getByRole('button', { name: 'Review unused items' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('checkbox', { name: /Old Mod/ }));
    fireEvent.click(within(dialog).getByRole('checkbox', { name: /Blocked Mod/ }));
    fireEvent.click(within(dialog).getByRole('button', { name: 'Remove selected…' }));

    expect(invokeMock).not.toHaveBeenCalledWith('games_content_remove', expect.anything());
    expect(within(dialog).getByText(/Remove 2 items\?/)).toBeInTheDocument();

    fireEvent.click(within(dialog).getByRole('button', { name: 'Remove 2 items' }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith('games_content_remove', {
        itemId: unusedItem.itemId,
        ignoreUnreadableSnapshots: false,
      });
    });
    expect(await screen.findByRole('status')).toHaveTextContent(
      'Removed 2 items. Reclaimed 2 object(s). Blocked Mod — Nothing was reclaimed: a linked object root.',
    );
    await waitFor(() => {
      expect(screen.getByTestId('unused-summary')).toHaveTextContent('Unused: 1 item, 2.0 MB');
    });
    expect(invokeMock).not.toHaveBeenCalledWith('games_content_remove', expect.objectContaining({
      itemId: ignoredItem.itemId,
    }));
  });

  it('offers to delete an unreadable snapshot that blocks removal', async () => {
    const snapshots = [{
      instanceId: 'mc1',
      snapshotId: 'snap-1',
      label: null,
      createdAt: '2026-01-01T00:00:00Z',
      path: 'instances/mc1/.agora_snapshots/snap-1.json',
      error: 'not valid json',
      command: 'agora snapshots delete mc1 snap-1',
      inTrash: false,
    }];
    let attempts = 0;
    invokeMock.mockImplementation(async (command: string) => {
      switch (command) {
        case 'games_library_list':
          return { items: [first], damaged: [], warnings: [] };
        case 'games_library_instances':
          return { instances: [], warnings: [] };
        case 'games_library_unused':
          return { items: [], totalSize: 0, warnings: [] };
        case 'games_library_icon':
          return null;
        case 'games_content_remove':
          attempts += 1;
          if (attempts === 1) {
            throw {
              code: 'UNREADABLE_SNAPSHOTS',
              message:
                "cannot remove item First item: snapshot 'snap-1' of instance 'mc1' cannot be read; "
                + 'delete it with: agora snapshots delete mc1 snap-1',
              details: { snapshots },
            };
          }
          return null;
        case 'delete_snapshot':
          return null;
        default:
          throw new Error(`unmocked command in a library test: ${command}`);
      }
    });

    render(<ConfirmProvider><LibraryPage /></ConfirmProvider>);
    await screen.findByText('A saved description.');
    await userEvent.click(screen.getByRole('button', { name: 'More for First item' }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /Remove from library/ }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Remove' }));

    expect(await screen.findByText(/Delete that snapshot/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Delete snapshot(s)' }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith('delete_snapshot', {
        instanceId: 'mc1',
        snapshotId: 'snap-1',
      });
    });
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith('games_content_remove', {
        itemId: first.itemId,
        ignoreUnreadableSnapshots: false,
      });
    });
    expect(await screen.findByRole('status')).toHaveTextContent('Removed First item.');
  });

  it('lets the person remove the item anyway after an unreadable snapshot', async () => {
    const snapshots = [{
      instanceId: 'mc1',
      snapshotId: 'snap-1',
      label: null,
      createdAt: '2026-01-01T00:00:00Z',
      path: 'instances/mc1/.agora_snapshots/snap-1.json',
      error: 'not valid json',
      command: 'agora snapshots delete mc1 snap-1',
      inTrash: false,
    }];
    let attempts = 0;
    invokeMock.mockImplementation(async (command: string) => {
      switch (command) {
        case 'games_library_list':
          return { items: [first], damaged: [], warnings: [] };
        case 'games_library_instances':
          return { instances: [], warnings: [] };
        case 'games_library_unused':
          return { items: [], totalSize: 0, warnings: [] };
        case 'games_library_icon':
          return null;
        case 'games_content_remove':
          attempts += 1;
          if (attempts === 1) {
            throw {
              code: 'UNREADABLE_SNAPSHOTS',
              message: "cannot remove item First item: snapshot 'snap-1' cannot be read",
              details: { snapshots },
            };
          }
          return null;
        default:
          throw new Error(`unmocked command in a library test: ${command}`);
      }
    });

    render(<ConfirmProvider><LibraryPage /></ConfirmProvider>);
    await screen.findByText('A saved description.');
    await userEvent.click(screen.getByRole('button', { name: 'More for First item' }));
    await userEvent.click(await screen.findByRole('menuitem', { name: /Remove from library/ }));
    fireEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Remove' }));

    expect(await screen.findByText(/Delete that snapshot/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(
      await screen.findByText(/Those snapshots will no longer be able to restore this item/),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Remove anyway' }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith('games_content_remove', {
        itemId: first.itemId,
        ignoreUnreadableSnapshots: true,
      });
    });
    expect(await screen.findByRole('status')).toHaveTextContent('Removed First item.');
  });

  it('offers nothing unused while an instance cannot be read, and says which one', async () => {
    invokeMock.mockImplementation(async (command: string) => {
      switch (command) {
        case 'games_library_list':
          return { items: [first], damaged: [], warnings: [] };
        case 'games_library_instances':
          return { instances: [], warnings: [] };
        case 'games_library_unused':
          return {
            items: [],
            totalSize: 0,
            warnings: ["Instance 'mc-broken' could not be read: bad manifest"],
          };
        default:
          throw new Error(`unmocked command in a library test: ${command}`);
      }
    });

    render(<LibraryPage />);
    const summary = await screen.findByTestId('unused-summary');
    expect(summary).toHaveTextContent('Unused items could not be checked');
    expect(summary).toHaveTextContent("Instance 'mc-broken' could not be read");
    expect(within(summary).queryByRole('button', { name: 'Review unused items' }))
      .not.toBeInTheDocument();
  });

  it('shows only the open game’s items, and the untied ones under their own filter', async () => {
    render(<LibraryPage />);
    await screen.findByText('A saved description.');
    expect(screen.getByText('First item')).toBeInTheDocument();
    expect(screen.getByText('Second item')).toBeInTheDocument();
    expect(screen.queryByText('Lux')).not.toBeInTheDocument();
    expect(screen.queryByText('Odin Patch')).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /Not tied to a game/ }));
    await waitFor(() => expect(screen.getByText('Odin Patch')).toBeInTheDocument());
    expect(screen.queryByText('First item')).not.toBeInTheDocument();
    expect(screen.getByText(/Nothing says which game these are for/)).toBeInTheDocument();
  });

  it('is the same page for another game, with its plugins and installer, in its own words', async () => {
    render(<LibraryPage gameId="skyrim-se" gameName="Skyrim SE" />);
    await screen.findByText('A lighting overhaul.');
    expect(screen.getByRole('heading', { name: 'Library' })).toBeInTheDocument();
    expect(screen.queryByText('First item')).not.toBeInTheDocument();
    const card = screen.getByTestId('library-item');
    expect(within(card).getByText('2 plugins')).toBeInTheDocument();
    expect(within(card).getByText('Lux - Orbis.esp')).toBeInTheDocument();
    expect(within(card).getByText('Installer')).toBeInTheDocument();
    expect(
      screen.getByText(
        'Mods saved for Skyrim SE, each is stored once and shared by your instances. Mods on this list can be reinstalled without internet.',
      ),
    ).toBeInTheDocument();
    // Fill in details is a Modrinth lookup: Minecraft's part, not this game's.
    expect(screen.queryByRole('button', { name: 'Fill in missing details' })).not.toBeInTheDocument();
  });

  it('records the game when an archive is added from that game’s Library', async () => {
    const base = invokeMock.getMockImplementation()!;
    invokeMock.mockImplementation(async (command: string, args: unknown) => {
      if (command === 'pick_open_file') return 'D:/mods/odin.zip';
      if (command === 'games_content_add') {
        return {
          status: 'added',
          item: { ...untied, fileCount: 1, gameIds: ['skyrim-se'], addedAtUnixMs: 1 },
          objectsNew: 1,
          objectsRestored: 0,
          skipped: [],
        };
      }
      return base(command, args);
    });
    render(<LibraryPage gameId="skyrim-se" gameName="Skyrim SE" />);
    await screen.findByText('A lighting overhaul.');
    fireEvent.click(screen.getByRole('button', { name: 'Add an archive…' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('games_content_add', {
        sourcePath: 'D:/mods/odin.zip',
        name: null,
        gameId: 'skyrim-se',
        withoutUnwritableNames: false,
      }),
    );
    expect(await screen.findByText('Added Odin Patch (1 file).')).toBeInTheDocument();
  });

  it('offers to add an archive without the files Windows cannot name', async () => {
    const base = invokeMock.getMockImplementation()!;
    let attempts = 0;
    invokeMock.mockImplementation(async (command: string, args: unknown) => {
      if (command === 'pick_open_file') return 'D:/mods/odin.zip';
      if (command === 'games_content_add') {
        const optIn = (args as { withoutUnwritableNames?: boolean }).withoutUnwritableNames;
        if (!optIn) {
          attempts += 1;
          throw {
            code: 'UNWRITABLE_NAMES',
            message: "Data/con.txt could not be written: a Windows reserved name",
            details: { entries: [{ name: 'Data/con.txt', reason: 'a Windows reserved name' }] },
          };
        }
        return {
          status: 'added',
          item: { ...untied, fileCount: 1, gameIds: ['skyrim-se'], addedAtUnixMs: 1 },
          objectsNew: 1,
          objectsRestored: 0,
          skipped: [{ name: 'Data/con.txt', reason: 'a Windows reserved name' }],
        };
      }
      return base(command, args);
    });
    render(<ConfirmProvider><LibraryPage gameId="skyrim-se" gameName="Skyrim SE" /></ConfirmProvider>);
    await screen.findByText('A lighting overhaul.');
    fireEvent.click(screen.getByRole('button', { name: 'Add an archive…' }));
    const dialog = await screen.findByRole('dialog');
    expect(dialog).toHaveTextContent('Data/con.txt');
    fireEvent.click(within(dialog).getByRole('button', { name: 'Add without them' }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('games_content_add', {
        sourcePath: 'D:/mods/odin.zip',
        name: null,
        gameId: 'skyrim-se',
        withoutUnwritableNames: true,
      }),
    );
    expect(await screen.findByText(/Data\/con\.txt/)).toBeInTheDocument();
    expect(attempts).toBe(1);
  });

  it('says the library could not be read and offers a retry, rather than looking empty', async () => {
    invokeMock.mockImplementation(async (command: string) => {
      if (command === 'games_library_list') throw { code: 'core_failed', message: 'The store index is damaged.' };
      if (command === 'games_library_instances') return { instances: [], warnings: [] };
      if (command === 'games_library_unused') return { items: [], totalSize: 0, warnings: [] };
      throw new Error(`unmocked command in a library test: ${command}`);
    });
    render(<ConfirmProvider><LibraryPage /></ConfirmProvider>);
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Your library could not be read: The store index is damaged.');
    expect(within(alert).getByRole('button', { name: 'Try again' })).toBeInTheDocument();
    expect(screen.queryByTestId('library-empty')).not.toBeInTheDocument();
  });
});
