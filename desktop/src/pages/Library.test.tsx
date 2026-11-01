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
      case 'games_library_icon':
        return null;
      case 'minecraft_add_stored_content':
        return {};
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
});
