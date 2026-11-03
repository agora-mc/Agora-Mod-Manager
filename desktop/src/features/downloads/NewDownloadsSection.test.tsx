import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async () => () => undefined),
}));

import { NewDownloadsSection } from './NewDownloadsSection';

const mod = {
  path: 'C:\\Users\\me\\Downloads\\JourneyMap-32274-5.9.7-1699999999.jar',
  fileName: 'JourneyMap-32274-5.9.7-1699999999.jar',
  size: 2 * 1024 * 1024,
  sha256: 'a'.repeat(64),
  gameId: 'minecraft',
  kind: 'mod',
  reason: 'has fabric.mod.json',
  nexusModId: '32274',
  alreadyInLibrary: false,
};

const already = {
  ...mod,
  path: 'C:\\Users\\me\\Downloads\\Known.zip',
  fileName: 'Known.zip',
  size: 900,
  sha256: 'b'.repeat(64),
  gameId: 'skyrim-se',
  kind: 'game content',
  reason: '`Cool.esp` matches the game\'s content layout',
  nexusModId: null,
  alreadyInLibrary: true,
};

function route(rows: unknown[], onAdd?: () => void) {
  let current = [...rows];
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case 'downloads_list':
        return current;
      case 'downloads_add':
        onAdd?.();
        current = current.filter((row) => (row as { sha256?: unknown }).sha256 !== args?.sha256);
        return { status: 'added', itemId: 'c'.repeat(64) };
      case 'downloads_ignore':
        current = current.filter((row) => (row as { sha256?: unknown }).sha256 !== args?.sha256);
        return null;
      default:
        throw new Error(`unmocked command in a downloads test: ${command}`);
    }
  });
}

describe('NewDownloadsSection', () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it('shows why each file was recognised and marks one already in the library', async () => {
    route([mod, already]);
    render(<NewDownloadsSection />);

    const rows = await screen.findAllByTestId('new-download');
    expect(rows).toHaveLength(2);

    const first = rows[0];
    expect(within(first).getByText(mod.fileName)).toBeInTheDocument();
    expect(within(first).getByText(/Minecraft · mod · has fabric\.mod\.json/)).toBeInTheDocument();
    expect(within(first).getByText('Nexus mod 32274')).toBeInTheDocument();
    expect(within(first).getByRole('button', { name: 'Add to library' })).toBeInTheDocument();

    const second = rows[1];
    expect(within(second).getByText('Already in your library.')).toBeInTheDocument();
    expect(within(second).queryByRole('button', { name: 'Add to library' })).not.toBeInTheDocument();
    expect(within(second).queryByText(/Nexus mod/)).not.toBeInTheDocument();
  });

  it('adds a download to the library and removes the row', async () => {
    let added = 0;
    route([mod], () => {
      added += 1;
    });
    render(<NewDownloadsSection />);

    fireEvent.click(await screen.findByRole('button', { name: 'Add to library' }));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('downloads_add', { sha256: mod.sha256 }),
    );
    expect(await screen.findByRole('status')).toHaveTextContent(
      `Added ${mod.fileName} to your library.`,
    );
    expect(added).toBe(1);
    await waitFor(() => {
      expect(screen.queryByTestId('new-download')).not.toBeInTheDocument();
    });
  });

  it('ignores a download by hash and removes the row', async () => {
    route([mod]);
    render(<NewDownloadsSection />);

    fireEvent.click(await screen.findByRole('button', { name: 'Ignore' }));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('downloads_ignore', { sha256: mod.sha256 }),
    );
    await waitFor(() => {
      expect(screen.queryByTestId('new-download')).not.toBeInTheDocument();
    });
  });

  it('renders nothing when there are no new downloads', async () => {
    route([]);
    const { container } = render(<NewDownloadsSection />);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith('downloads_list'));
    expect(container).toBeEmptyDOMElement();
  });
});
