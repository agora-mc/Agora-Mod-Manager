/**
 * The Tools tab's own list: candidates offered by Find tools, the ones already added, and what
 * "Add selected" does with the ticked rows.
 */
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { InstanceToolsTab } from './InstanceToolsTab';
import {
  addUserTool,
  findUserTools,
  gameToolSwapStatus,
  listGameTools,
  listUserTools,
  removeUserTool,
  type ToolCandidate,
} from './api';

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>();
  return {
    ...actual,
    addUserTool: vi.fn(),
    findUserTools: vi.fn(),
    gameToolSwapStatus: vi.fn(),
    listGameTools: vi.fn(),
    listUserTools: vi.fn(),
    removeUserTool: vi.fn(),
    restoreGameToolSwap: vi.fn(),
  };
});

const unknown: ToolCandidate = {
  exe: 'Data/tools/FNIS.exe',
  modId: 'aaaa1111',
  modName: 'FNIS',
  knownToolId: null,
  suggestedName: 'FNIS',
  alreadyAdded: false,
};

const known: ToolCandidate = {
  exe: 'Data/CalienteTools/BodySlide/BodySlide x64.exe',
  modId: 'bbbb2222',
  modName: 'BodySlide and Outfit Studio',
  knownToolId: 'bodyslide',
  suggestedName: 'BodySlide',
  alreadyAdded: false,
};

const added: ToolCandidate = {
  exe: 'Data/tools/Old.exe',
  modId: 'cccc3333',
  modName: 'Old Tools',
  knownToolId: null,
  suggestedName: 'Old',
  alreadyAdded: true,
};

beforeEach(() => {
  vi.mocked(listGameTools).mockReset().mockResolvedValue({ instanceId: 'inst-1', tools: [] });
  vi.mocked(listUserTools).mockReset().mockResolvedValue({ instanceId: 'inst-1', tools: [] });
  vi.mocked(gameToolSwapStatus).mockReset().mockResolvedValue({ interrupted: null });
  vi.mocked(findUserTools).mockReset().mockResolvedValue([unknown, known, added]);
  vi.mocked(addUserTool).mockReset().mockImplementation(async (_id, tool) => ({
    id: 'made-1',
    name: tool.name,
    exe: tool.exe,
    args: tool.args,
    knownToolId: null,
  }));
  vi.mocked(removeUserTool).mockReset();
});

function mount() {
  return render(<InstanceToolsTab gameId="skyrim-se" instanceId="inst-1" />);
}

describe('Find tools', () => {
  it('offers the candidates, greys out one already added, and adds only the ticked rows', async () => {
    mount();
    await screen.findByTestId('user-tools-empty');
    fireEvent.click(screen.getByRole('button', { name: 'Find tools' }));

    const rows = await screen.findAllByTestId('tool-candidate');
    expect(rows).toHaveLength(3);
    expect(within(rows[0]).getByDisplayValue('FNIS')).toBeInTheDocument();
    expect(within(rows[0]).getByText(/From FNIS/)).toBeInTheDocument();
    expect(within(rows[1]).getByText('Known tool')).toBeInTheDocument();
    expect(within(rows[2]).getByText('Already added')).toBeInTheDocument();

    const addOld = within(rows[2]).getByRole('checkbox');
    expect(addOld).toBeDisabled();

    const addBodySlide = within(rows[1]).getByRole('checkbox', { name: /BodySlide/ });
    fireEvent.click(addBodySlide);
    fireEvent.click(screen.getByRole('button', { name: 'Add selected' }));

    await waitFor(() => expect(addUserTool).toHaveBeenCalledTimes(1));
    expect(addUserTool).toHaveBeenCalledWith('inst-1', {
      name: 'FNIS',
      exe: 'Data/tools/FNIS.exe',
      args: [],
    });
    expect(await screen.findByText('Added 1 tool.')).toBeInTheDocument();
  });

  it('says when the mods hold no tools', async () => {
    vi.mocked(findUserTools).mockResolvedValue([]);
    mount();
    await screen.findByTestId('user-tools-empty');
    fireEvent.click(screen.getByRole('button', { name: 'Find tools' }));
    expect(await screen.findByText('No tools found in this instance’s mods')).toBeInTheDocument();
  });

  it('names the failure when the search cannot run, rather than showing no tools', async () => {
    vi.mocked(findUserTools).mockRejectedValue({ code: 'core_failed', message: 'The base is missing.' });
    mount();
    await screen.findByTestId('user-tools-empty');
    fireEvent.click(screen.getByRole('button', { name: 'Find tools' }));
    const alert = await screen.findByRole('alert');
    expect(alert).toHaveTextContent('Your mods could not be searched: The base is missing.');
    expect(screen.queryByText('No tools found in this instance’s mods')).toBeNull();
  });
});

describe('the list of the person’s own tools', () => {
  it('lists a tool with its executable and removes it', async () => {
    vi.mocked(listUserTools).mockResolvedValue({
      instanceId: 'inst-1',
      tools: [{ id: 'tool-1', name: 'FNIS', exe: 'Data/tools/FNIS.exe', args: ['-a'], knownToolId: null }],
    });
    vi.mocked(removeUserTool).mockResolvedValue({ instanceId: 'inst-1', tools: [] });
    mount();
    const card = await screen.findByTestId('user-tool-card');
    expect(within(card).getByText('Data/tools/FNIS.exe')).toBeInTheDocument();
    expect(within(card).getByText('Starts with: -a')).toBeInTheDocument();
    fireEvent.click(within(card).getByRole('button', { name: 'Remove' }));
    await waitFor(() => expect(removeUserTool).toHaveBeenCalledWith('inst-1', 'tool-1'));
  });

  it('says why the list cannot be read instead of showing it as empty', async () => {
    vi.mocked(listUserTools).mockRejectedValue({ code: 'core_failed', message: 'The file is malformed.' });
    mount();
    const alert = await screen.findByTestId('user-tools-error');
    expect(alert).toHaveTextContent('Your own tools could not be listed: The file is malformed.');
    expect(screen.queryByTestId('user-tools-empty')).toBeNull();
  });
});
