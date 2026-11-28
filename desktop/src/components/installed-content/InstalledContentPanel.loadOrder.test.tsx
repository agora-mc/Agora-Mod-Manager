import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { InstalledContentPanel } from './InstalledContentPanel';
import type { ContentColumn } from './types';
import type { InstalledContentRow } from '../../lib/tauri';

vi.mock('../../lib/tauri', () => ({
  formatError: (error: unknown) => error instanceof Error ? error.message : String(error),
}));

function row(name: string, position: number, enabled = true): InstalledContentRow {
  return {
    key: `content:${name}`,
    filename: '',
    display_name: name,
    version: null,
    content_type: 'mod',
    enabled,
    installed_at: '',
    source: 'archive',
    source_label: 'Archive: item.zip',
    pack_managed: false,
    installed_as_dependency: false,
    update_pinned: false,
    source_url: null,
    registry_id: null,
    modrinth_id: null,
    mod_jar_id: null,
    loader_mod_id: null,
    size_bytes: null,
    file_present: true,
    file_state: 'ok',
    resolved_path: null,
    author: null,
    categories: [],
    icon_url: null,
    curation_status: 'unknown',
    agora_score: null,
    modrinth_downloads: null,
    metadata_status: 'unavailable',
    world_sync: null,
    ...({ position } as Partial<InstalledContentRow>),
  } as InstalledContentRow;
}

const defaultOnMove = vi.fn(async (_row: InstalledContentRow, _position: number) => true);
const gameColumns: ContentColumn[] = ['enabled', 'name', 'source', 'actions'];

function gamePanelProps(rows: InstalledContentRow[], onMove = defaultOnMove) {
  const positions = new Map(rows.map((entry) => [entry.key, (entry as InstalledContentRow & { position: number }).position]));
  return {
    contentType: 'mod' as const,
    title: 'Content',
    contentLabel: 'content',
    description: 'What this instance uses, in load order.',
    rows,
    locked: false,
    availableColumns: gameColumns,
    columnLabels: { name: 'Name' },
    defaultColumns: gameColumns,
    availableFilters: { categories: false, curation: false, source: true, enabled: true, group: false },
    rowTestId: 'installed-content-row',
    onToggle: vi.fn(async () => true),
    onBulkToggle: vi.fn(async () => true),
    onBulkRemove: vi.fn(() => true),
    onRemove: vi.fn(),
    loadOrder: {
      positionForRow: (entry: InstalledContentRow) => positions.get(entry.key) ?? 0,
      onMove,
    },
  };
}

function dataTransfer(key = '') {
  return {
    effectAllowed: '',
    dropEffect: '',
    setData: vi.fn(),
    getData: vi.fn(() => key),
  };
}

beforeEach(() => {
  localStorage.clear();
});

describe('load order mode', () => {
  it('shows the true position and defaults to lowest first even when the props are unsorted', () => {
    const second = row('Second', 2);
    const first = row('First', 1);
    render(<InstalledContentPanel {...gamePanelProps([second, first])} />);

    const rows = screen.getAllByTestId('installed-content-row');
    expect(rows[0]).toHaveTextContent('First');
    expect(rows[1]).toHaveTextContent('Second');
    expect(screen.getAllByTestId('load-order-number').map((element) => element.textContent)).toEqual(['1.', '2.']);
  });

  it('keeps numbers but hides reordering when sorted another way, and offers a way back', () => {
    const onMove = vi.fn(async (_row: InstalledContentRow, _position: number) => true);
    const rows = [row('First', 1), row('Second', 2)];
    render(<InstalledContentPanel {...gamePanelProps(rows, onMove)} />);

    fireEvent.click(screen.getByRole('button', { name: 'Name' }));

    expect(screen.getByTestId('reorder-blocked')).toHaveTextContent('Reordering is off while the list is sorted another way.');
    expect(screen.getByRole('button', { name: 'Move First up' })).toBeDisabled();
    expect(screen.getAllByTestId('load-order-number').map((element) => element.textContent)).toEqual(['1.', '2.']);

    fireEvent.click(screen.getByRole('button', { name: 'Back to load order' }));
    expect(screen.getByRole('button', { name: 'Move Second up' })).toBeInTheDocument();
  });

  it('makes a reorder impossible while the list is searched or filtered', () => {
    const onMove = vi.fn(async (_row: InstalledContentRow, _position: number) => true);
    const rows = [row('First', 1), row('Second', 2)];
    render(<InstalledContentPanel {...gamePanelProps(rows, onMove)} />);

    fireEvent.change(screen.getByLabelText('Search installed content'), { target: { value: 'First' } });

    expect(screen.getByTestId('reorder-blocked')).toHaveTextContent('Reordering is off while the list is searched or filtered.');
    expect(screen.getByRole('button', { name: 'Move First up' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Move First down' })).toBeDisabled();
    expect(onMove).not.toHaveBeenCalled();
  });

  it('changes nothing when a row is dropped onto itself', () => {
    const onMove = vi.fn(async (_row: InstalledContentRow, _position: number) => true);
    const rows = [row('First', 1), row('Second', 2)];
    render(<InstalledContentPanel {...gamePanelProps(rows, onMove)} />);

    const first = screen.getAllByTestId('installed-content-row')[0];
    fireEvent.dragStart(first, { dataTransfer: dataTransfer('content:First') });
    fireEvent.dragOver(first, { dataTransfer: dataTransfer('content:First') });
    fireEvent.drop(first, { dataTransfer: dataTransfer('content:First') });

    expect(onMove).not.toHaveBeenCalled();
  });

  it('moves a dropped row to the dropped row’s true position', async () => {
    const onMove = vi.fn(async (_row: InstalledContentRow, _position: number) => true);
    const rows = [row('First', 1), row('Second', 2), row('Third', 3)];
    render(<InstalledContentPanel {...gamePanelProps(rows, onMove)} />);

    const [first, , third] = screen.getAllByTestId('installed-content-row');
    fireEvent.dragStart(first, { dataTransfer: dataTransfer('content:First') });
    fireEvent.dragOver(third, { dataTransfer: dataTransfer('content:First') });
    fireEvent.drop(third, { dataTransfer: dataTransfer('content:First') });

    await waitFor(() => expect(onMove).toHaveBeenCalledTimes(1));
    expect(onMove.mock.calls[0][1]).toBe(3);
  });

  it('shows the error and keeps the true order when a reorder fails', async () => {
    const onMove = vi.fn(async (_row: InstalledContentRow, _position: number) => { throw new Error('Layer file is damaged.'); });
    const onError = vi.fn();
    const rows = [row('First', 1), row('Second', 2), row('Third', 3)];
    render(<InstalledContentPanel {...gamePanelProps(rows, onMove)} onError={onError} />);

    const [first, , third] = screen.getAllByTestId('installed-content-row');
    fireEvent.dragStart(first, { dataTransfer: dataTransfer('content:First') });
    fireEvent.dragOver(third, { dataTransfer: dataTransfer('content:First') });
    fireEvent.drop(third, { dataTransfer: dataTransfer('content:First') });

    await waitFor(() => expect(onError).toHaveBeenCalledWith('Layer file is damaged.'));
    expect(screen.getAllByTestId('installed-content-row').map((element) => element.textContent)).toEqual([
      expect.stringContaining('First'),
      expect.stringContaining('Second'),
      expect.stringContaining('Third'),
    ]);
  });

  it('keeps Move up and Move down for people who are not dragging', async () => {
    const onMove = vi.fn(async (_row: InstalledContentRow, _position: number) => true);
    const rows = [row('First', 1), row('Second', 2)];
    render(<InstalledContentPanel {...gamePanelProps(rows, onMove)} />);

    expect(screen.getByRole('button', { name: 'Move First up' })).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: 'Move Second up' }));
    await waitFor(() => expect(onMove).toHaveBeenCalledWith(expect.objectContaining({ key: 'content:Second' }), 1));
  });

  it('leaves Minecraft’s list without numbers or move controls', () => {
    const rows = [row('First', 1), row('Second', 2)];
    render(
      <InstalledContentPanel
        {...gamePanelProps(rows)}
        loadOrder={undefined}
        availableColumns={undefined}
        columnLabels={undefined}
        defaultColumns={undefined}
        availableFilters={undefined}
      />,
    );

    expect(screen.queryAllByTestId('load-order-number')).toHaveLength(0);
    expect(screen.queryByRole('button', { name: /^Move / })).toBeNull();
    expect(screen.queryByTestId('reorder-blocked')).toBeNull();
  });

  it('renders a 370-row list with true positions', () => {
    const rows = Array.from({ length: 370 }, (_, index) => row(`Item ${index + 1}`, index + 1));
    const started = performance.now();
    render(<InstalledContentPanel {...gamePanelProps(rows)} />);
    const elapsed = performance.now() - started;

    expect(screen.getAllByTestId('installed-content-row')).toHaveLength(370);
    const numbers = screen.getAllByTestId('load-order-number').map((element) => element.textContent);
    expect(numbers[0]).toBe('1.');
    expect(numbers[369]).toBe('370.');
    console.log(`370-row panel render: ${Math.round(elapsed)} ms`);
  });
});
