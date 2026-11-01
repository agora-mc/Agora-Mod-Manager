import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));

import { LibraryPage } from './Library';

const first = {
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

const second = {
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

function route() {
  invokeMock.mockImplementation(async (command: string) => {
    switch (command) {
      case 'games_library_list':
        return { items: [first, second], damaged: [], warnings: [] };
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
    expect(screen.getByText('File: first.jar')).toBeInTheDocument();
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

  it('removes only the unused items the user selects and confirms', async () => {
    const unusedItem = {
      itemId: 'e'.repeat(64),
      name: 'Old Mod',
      totalSize: 2 * 1024 * 1024,
      gameId: 'minecraft',
    };
    let removed = false;
    invokeMock.mockImplementation(async (command: string) => {
      switch (command) {
        case 'games_library_list':
          return { items: [first], damaged: [], warnings: [] };
        case 'games_library_instances':
          return { instances: [], warnings: [] };
        case 'games_library_unused':
          return removed
            ? { items: [], totalSize: 0, warnings: [] }
            : { items: [unusedItem], totalSize: unusedItem.totalSize, warnings: [] };
        case 'games_content_remove':
          removed = true;
          return null;
        default:
          throw new Error(`unmocked command in a library test: ${command}`);
      }
    });

    render(<LibraryPage />);
    expect(await screen.findByTestId('unused-summary')).toHaveTextContent(
      'Unused: 1 item, 2.0 MB',
    );

    fireEvent.click(screen.getByRole('button', { name: 'Review unused items' }));
    const dialog = await screen.findByRole('dialog');
    fireEvent.click(within(dialog).getByRole('checkbox'));
    fireEvent.click(within(dialog).getByRole('button', { name: 'Remove selected…' }));

    expect(invokeMock).not.toHaveBeenCalledWith('games_content_remove', expect.anything());
    expect(within(dialog).getByText(/Remove 1 item\?/)).toBeInTheDocument();

    fireEvent.click(within(dialog).getByRole('button', { name: 'Remove 1 item' }));

    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith('games_content_remove', {
        itemId: unusedItem.itemId,
      });
    });
    expect(await screen.findByRole('status')).toHaveTextContent('Removed 1 item.');
    await waitFor(() => {
      expect(screen.queryByTestId('unused-summary')).not.toBeInTheDocument();
    });
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
});
